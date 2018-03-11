#!/usr/bin/env python
# coding:utf-8
# Contributor:
#      fffonion        <fffonion@gmail.com>

import re
import math
import time
import random
import requests
from requests.adapters import HTTPAdapter
import traceback
from threading import Thread, RLock
from . import util
from .const import *
from .i18n import i18n
from .proxy import PoolException
if PY3K:
    from queue import Queue, Empty
    from urllib.parse import urlparse, urlunparse
else:
    from Queue import Queue, Empty
    from urlparse import urlparse, urlunparse

# pinfo = {'http':'socks5://127.0.0.1:16963', 'https':'socks5://127.0.0.1:16963'}

class _FakeResponse(object):
    def __init__(self, url):
        self.status_code = 600
        self.content = None
        self.url = self._real_url = url
        self.headers = {}

class FallbackIpAdapter(HTTPAdapter):
    def __init__(self, ip_map=FALLBACK_IP_MAP, **kwargs):
        self.ip_map = ip_map
        kwargs.update({'max_retries': 1})
        requests.adapters.HTTPAdapter.__init__(self, **kwargs)

    # override
    def get_connection(self, url, proxies=None):
        if not proxies:
            parsed = urlparse(url)
            _hostname = parsed.hostname
            _scheme = parsed.scheme
            if _hostname in self.ip_map:
                _parsed = list(parsed)
                # alter the hostname
                _hostname = '%s%s' % (random.choice(self.ip_map[_hostname]),
                                        (":%d" % parsed.port) if parsed.port else "")
                _scheme = 'https'
            return self.poolmanager.connection_from_host(_hostname, parsed.port, scheme=_scheme,
                                                            pool_kwargs={'assert_hostname': parsed.hostname})
        else:
            # fallback
            return requests.adapters.HTTPAdapter.get_connection(self, url, proxies)
    
    def add_headers(self, request, **kwargs):
        if not request.headers.get('Host'):
            parsed = urlparse(request.url)
            request.headers['Host'] = parsed.hostname
    
    def cert_verify(self, conn, url, verify, cert):
        # let the super run verify process
        if url.startswith('http://'):
            url = "https://%s" % url[7:]
        return requests.adapters.HTTPAdapter.cert_verify(self, conn, url, verify, cert)

class HttpReq(object):
    def __init__(self, headers = {}, proxy = None, proxy_policy = None, retry = 10, timeout = 20, logger = None, tname = "main"):
        self.session = requests.Session()
        self.session.headers = headers
        for u in ('forums.e-hentai.org', 'e-hentai.org', 'exhentai.org'):
            self.session.mount('http://%s' % u, FallbackIpAdapter())
            self.session.mount('https://%s' % u, FallbackIpAdapter())
        self.session.mount('http://', HTTPAdapter(max_retries=0))
        self.retry = retry
        self.timeout = timeout
        self.proxy = proxy
        self.proxy_policy = proxy_policy
        self.logger = logger
        self.tname = tname

    def request(self, method, url, _filter, suc, fail, data=None, stream_cb=None):
        retry = 0
        url_history = [url]
        while retry < self.retry:
            try:
                headers = {}
                # if proxy_policy is set and match current url, use proxy
                if self.proxy and self.proxy_policy and self.proxy_policy.match(url):
                    f, __not_good = self.proxy.proxied_request(self.session)
                else:
                    f = self.session.request
                r = f(method, url,
                    allow_redirects=False,
                    data=data,
                    timeout=self.timeout,
                    stream=stream_cb != None)
            except requests.RequestException as ex:
                self.logger.warning("%s-%s %s %s: %s" % (i18n.THREAD, self.tname, method, url, ex))
                time.sleep(random.random() + 0.618)
            else:
                if r.headers.get('content-length'):
                    r.content_length = int(r.headers.get('content-length'))
                elif not stream_cb:
                    r.content_length = len(r.content)
                else:
                    r.content_length = 0
                self.logger.verbose("%s-%s %s %s %d %d" % (i18n.THREAD, self.tname, method, url, r.status_code, r.content_length))
                # if it's a redirect, 3xx
                if r.status_code > 300 and r.status_code < 400:
                    _new_url = r.headers.get("location")
                    if _new_url:
                        url_history.append(url)
                        if len(url_history) > DEFAULT_MAX_REDIRECTS:
                            self.logger.warning("%s-%s %s %s: too many redirects" % (i18n.THREAD, self.tname, method, url))
                            return _filter(_FakeResponse(url_history[0]), suc, fail)
                        url = _new_url
                        continue
                # intercept some error to see if we can change IP
                if self.proxy and r.content_length < 1024 and \
                    re.match("Your IP address has been temporarily banned", r.text):
                    _t = util.parse_human_time(r.text)
                    self.logger.warn(i18n.PROXY_DISABLE_BANNED % _t)
                    # fail this proxy immediately and set expire time
                    __not_good(expire = _t)
                    continue
                
                r.encoding = "utf-8"
                # r._text_bytes = r.text.encode("utf-8")
                r._real_url = url_history[-1]

                r.iter_content_cb = stream_cb

                return _filter(r, suc, fail)
            retry += 1
        return _filter(_FakeResponse(url_history[0]), suc, fail)



class HttpWorker(Thread, HttpReq):
    def __init__(self, tname, task_queue, flt, suc, fail, headers={}, proxy=None, proxy_policy=None,
            retry=3, timeout=10, logger=None, keep_alive=None, stream_mode=False):
        """
        Construct a new 'HttpWorker' obkect

        :param tname: The name of this http worker
        :param task_queue: The task Queue instance
        :param flt: the filter function
        :param suc: the function to call when succeeded
        :param fail: the function to call when failed
        :param headers: custom HTTP headers
        :param proxy: proxy dict
        :param proxy_policy: a function to determine whether proxy should be used
        :param retry: retry count
        :param timeout: timeout in seconds
        :param logger: the Logger instance
        :param keep_alive: the callback to send keep alive
        :param stream_mode: set the request to use stream mode, keep_alive will be called every iteration
        :return: returns nothing
        """
        HttpReq.__init__(self, headers, proxy, proxy_policy, retry, timeout, logger, tname = tname)
        Thread.__init__(self, name = tname)
        Thread.setDaemon(self, True)
        self.task_queue = task_queue
        self.logger = logger
        self._keepalive = keep_alive
        self._exit = lambda x: False
        self.flt = flt
        self.f_suc = suc
        self.f_fail = fail
        self.stream_mode = stream_mode
        self.run_once = False

    def _finish_queue(self, *args):
        # exit if current queue is finished
        return self.run_once and self.task_queue.empty()

    def run(self):
        self.logger.verbose("t-%s start" % self.name)
        _stream_cb = None
        if self.stream_mode:
            _stream_cb = lambda x:self._keepalive(self)
        while not self._keepalive(self) and not self._exit(self):
            try:
                url = self.task_queue.get(False)
            except Empty:
                time.sleep(1)
                continue
            self.run_once = True
            try:
                self.request("GET", url, self.flt, self.f_suc, self.f_fail, stream_cb=_stream_cb)
            except PoolException as ex:
                self.logger.warning("%s-%s %s" % (i18n.THREAD, self.tname, str(ex)))
                break
            except Exception as ex:
                self.logger.warning(i18n.THREAD_UNCAUGHT_EXCEPTION % (self.tname, traceback.format_exc()))
                self.flt(_FakeResponse(url), self.f_suc, self.f_fail)
        # notify monitor the last time
        self.logger.verbose("t-%s exit" % self.name)
        self._keepalive(self, _exit = True)

class ArchiveWorker(Thread):
    # this worker is not managed by monitor
    def __init__(self, logger, task, exit_check = None):
        Thread.__init__(self, name = "archiver%s" % task.guid)
        Thread.setDaemon(self, True)
        self.logger = logger
        self.task = task
        self._exit = lambda x: False

    def run(self):
        while self.task.state < TASK_STATE_FINISHED:
            if self._exit(self) or self.task.state in (TASK_STATE_PAUSED, TASK_STATE_FAILED):
                return
            time.sleep(1)
        self.logger.info(i18n.TASK_START_MAKE_ARCHIVE % self.task.guid)
        self.task.state = TASK_STATE_MAKE_ARCHIVE
        t = time.time()
        try:
            pth = self.task.make_archive()
        except Exception as ex:
            self.task.state = TASK_STATE_FAILED
            self.logger.error(i18n.TASK_ERROR % (self.task.guid, i18n.c(ERR_CANNOT_MAKE_ARCHIVE) % traceback.format_exc()))
        else:
            self.task.state = TASK_STATE_FINISHED
            self.logger.info(i18n.TASK_MAKE_ARCHIVE_FINISHED % (self.task.guid, pth, time.time() - t))


class Monitor(Thread):
    def __init__(self, req, proxy, logger, task, exit_check=None, ignored_errors=[]):
        Thread.__init__(self, name = "monitor%s" % task.guid)
        Thread.setDaemon(self, True)
        # the count of votes per error code
        self.vote_result = {}
        # the error code to be ignored
        self.vote_cleared = set().union(ignored_errors)
        self.thread_last_seen = {}
        self.dctlock = RLock()
        self.votelock = RLock()
        self.thread_ref = {}
        self.thread_zombie = set()
        # HttpReq instance
        self.req = req
        # proxy.Pool instance
        self.proxy = proxy
        self.logger = logger
        self.task = task
        self._exit = exit_check if exit_check else lambda x: False
        self._cleaning_up = False
        if os.name == "nt":
            self.set_title = lambda s:os.system("TITLE %s" % (
                s if PY3K else s.encode(CODEPAGE, 'replace')))
        elif os.name == 'posix':
            import sys
            self.set_title = lambda s:sys.stdout.write("\033]2;%s\007" % (
                s if PY3K else s.encode(CODEPAGE, 'replace')))

    def set_vote_ns(self, tnames):
        t = time.time()
        self.thread_last_seen = {k:t for k in tnames}

    def vote(self, tname, code):
        # thread_id, result_code
        self.votelock.acquire()
        if code != ERR_NO_ERROR:
            self.logger.verbose("t-%s vote:%s" % (tname, code))
        if code not in self.vote_result:
            self.vote_result[code] = 1
        else:
            self.vote_result[code] += 1
        self.votelock.release()

    def wrk_keepalive(self, wrk_thread, _exit = False):
        tname = wrk_thread.name
        if tname in self.thread_zombie:
            self.thread_zombie.remove(tname)
        # all image downloaded
        # task is finished or failed
        # monitor is exiting or worker notify its exit
        _ = self.task.meta['finished'] == self.task.meta['total'] or \
            self.task.state in (TASK_STATE_FINISHED, TASK_STATE_FAILED) or \
            self._exit("mon") or _exit
        # self.logger.verbose("mon#%s %s ask, %s, %s" % (self.task.guid, tname, _,
        #    self.thread_last_seen))
        if _ or not wrk_thread.is_alive():
            self.dctlock.acquire()
            if tname in self.thread_last_seen:
                del self.thread_last_seen[tname]
            if tname in self.thread_ref:
                del self.thread_ref[tname]
            self.dctlock.release()
        else:
            self.thread_last_seen[tname] = time.time()
            if tname not in self.thread_ref:
                self.thread_ref[tname] = wrk_thread
        return _

    # def _rescan_pages(self):
    #     # not using
    #     # throw away existing page urls
    #     while True:
    #         try:
    #             self.task.page_q.get(False)
    #         except Empty:
    #             break
    #     # put page into task.list_q
    #     [self.task.list_q.put("%s/?p=%d" % (self.task.url, x)
    #         for x in range(1, 1 + int(math.ceil(self.task.meta['total']/20.0))))
    #     ]
    #     print(self.task.list_q.qsize())

    def _check_vote(self):
        if False and ERR_IMAGE_RESAMPLED in self.vote_result and ERR_IMAGE_RESAMPLED not in self.vote_cleared:
            self.logger.warning(i18n.TASK_START_PAGE_RESCAN % self.task.guid)
            self._rescan_pages()
            self.task.meta['has_ori'] = True
            self.vote_cleared.add(ERR_IMAGE_RESAMPLED)
        elif ERR_QUOTA_EXCEEDED in self.vote_result and \
            ERR_QUOTA_EXCEEDED not in self.vote_cleared and \
            self.vote_result[ERR_QUOTA_EXCEEDED] >= len(self.thread_last_seen):
            self.logger.error(i18n.TASK_STOP_QUOTA_EXCEEDED % self.task.guid)
            self.task.state = TASK_STATE_FAILED

    def run(self):
        CHECK_INTERVAL = 10
        STUCK_INTERVAL = 90
        intv = 0
        self.set_title(i18n.TASK_START % self.task.guid)
        last_change = time.time()
        last_finished = -1
        while len(self.thread_last_seen) > 0:
            intv += 1
            self._check_vote()
            for k in list(self.thread_last_seen.keys()):
                if time.time() - self.thread_last_seen[k] > 30:
                    if k in self.thread_ref and self.thread_ref[k].is_alive():
                        self.logger.warning(i18n.THREAD_MAY_BECOME_ZOMBIE % k)
                        self.thread_zombie.add(k)
                    else:
                        self.logger.warning(i18n.THREAD_SWEEP_OUT % k)
                    del self.thread_last_seen[k]
            if intv == CHECK_INTERVAL:
                _ = "%s %dR/%dZ, %s %dR/%dD" % (
                    i18n.THREAD,
                    len(self.thread_last_seen), len(self.thread_zombie),
                    i18n.QUEUE,
                    self.task.img_q.qsize(),
                    self.task.meta['finished'])
                self.logger.info(_)
                self.set_title(_)
                intv = 0
                # if not downloading any new images in 1.5 min, exit
                if last_finished != self.task.meta['finished']:
                    last_change = time.time()
                    last_finished = self.task.meta['finished']
                else:
                    if time.time() - last_change > STUCK_INTERVAL:
                        self.logger.warning(i18n.TASK_STUCK % self.task.guid)
                        break
            time.sleep(0.5)
        if self.task.meta['finished'] == self.task.meta['total']:
            _err = self.task.rename_fname()
            if _err:
                self.logger.warning(i18n.XEH_RENAME_HAS_ERRORS % (
                    "\n".join(map(lambda x:"%s => %s : %s" % x, _err))
                ))
            self.set_title(i18n.TASK_FINISHED % self.task.guid)
            self.logger.info(i18n.TASK_FINISHED % self.task.guid)
            self.task.state = TASK_STATE_FINISHED
        self.task.cleanup()

if __name__ == '__main__':
    print(HttpReq().request("GET", "https://ipip.tk", lambda x:x, None, None))
