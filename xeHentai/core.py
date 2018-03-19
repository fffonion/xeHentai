#!/usr/bin/env python
# coding:utf-8
# Contributor:
#      fffonion        <fffonion@gmail.com>

from __future__ import absolute_import
import os
import re
import sys
import math
import json
import time
import traceback
from .task import Task
from . import util
from . import proxy
from . import filters
from .rpc import RPCServer
from .i18n import i18n
from .util import logger
from .const import *
from .const import __version__
from .worker import *
if PY3K:
    from queue import Queue, Empty
else:
    from Queue import Queue, Empty

from . import config as default_config
sys.path.insert(1, FILEPATH)
try:
    import config
except ImportError:
    config = default_config
sys.path.pop(1)

class xeHentai(object):
    def __init__(self):
        self.verstr = "%.3f%s" % (__version__, '-dev' if DEVELOPMENT else "")
        self.logger = logger.Logger()
        self._exit = False
        self.tasks = Queue() # for queueing, stores gid only
        self.last_task_guid = None
        self._all_tasks = {} # for saving states
        self._all_threads = [[] for i in range(20)]
        self.cfg = {k:v for k,v in default_config.__dict__.items() if not k.startswith("_")}
        # note that ignored_errors are overwritten using val from custom config
        self.cfg.update({k:v for k,v in config.__dict__.items() if not k.startswith("_")})
        self.proxy = None
        self.cookies = {"nw": "1"}
        self.headers = {
            'User-Agent': util.make_ua(),
            'Accept-Charset': 'utf-8;q=0.7,*;q=0.7',
            'Accept': 'text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8',
            'Connection': 'keep-alive'
        }
        self.has_login = False
        self.load_session()
        self.rpc = None

    def update_config(self, **cfg_dict):
        self.cfg.update({k:v for k, v in cfg_dict.items() if k in cfg_dict and k not in ('ignored_errors',)})
        # merge ignored errors list
        if 'ignored_errors' in cfg_dict and cfg_dict['ignored_errors']:
            self.cfg['ignored_errors'] = list(set(self.cfg['ignored_errors'] + cfg_dict['ignored_errors']))
        self.logger.set_level(logger.Logger.WARNING - self.cfg['log_verbose'])
        self.logger.verbose("cfg %s" % self.cfg)
        if cfg_dict['proxy']:
            if not self.proxy: # else we keep it None
                self.proxy = proxy.Pool()
            for p in self.cfg['proxy']:
                try:
                    self.proxy.add_proxy(p)
                except Exception as ex:
                    self.logger.warning(traceback.format_exc())
            self.logger.debug(i18n.PROXY_CANDIDATE_CNT % len(self.proxy.proxies))
        if cfg_dict['dir'] and not os.path.exists(cfg_dict['dir']):
            try:
                os.makedirs(cfg_dict['dir'])
            except OSError as ex:  # Python >2.5
                self.logger.error(i18n.ERR_CANNOT_CREATE_DIR % cfg_dict['dir'])
        if not self.rpc and self.cfg['rpc_port'] and self.cfg['rpc_interface']:
            self.rpc = RPCServer(self, (self.cfg['rpc_interface'], int(self.cfg['rpc_port'])),
                secret = None if 'rpc_secret' not in self.cfg else self.cfg['rpc_secret'],
                logger = self.logger)
            if not RE_LOCAL_ADDR.match(self.cfg['rpc_interface']) and \
                not self.cfg['rpc_secret']:
                self.logger.warning(i18n.RPC_TOO_OPEN % self.cfg['rpc_interface'])
            self.rpc.start()
        self.logger.set_logfile(self.cfg['log_path'])
        return ERR_NO_ERROR, ""

    def _get_httpreq(self, proxy_policy):
        return HttpReq(self.headers, logger = self.logger, proxy = self.proxy, proxy_policy = proxy_policy)

    def _get_httpworker(self, tid, task_q, flt, suc, fail, keep_alive, proxy_policy, timeout, stream_mode):
        return HttpWorker(tid, task_q, flt, suc, fail,
            headers = self.headers, proxy = self.proxy, logger = self.logger,
            keep_alive = keep_alive, proxy_policy = proxy_policy, timeout = timeout, stream_mode = stream_mode)

    def add_task(self, url, **cfg_dict):
        url = url.strip()
        cfg = {k:v for k, v in self.cfg.items() if k in (
            "dir", "download_ori", "download_thread_cnt", "scan_thread_cnt",
            "proxy_image", "proxy_image_only", "ignored_errors",
            "rename_ori", "make_archive", "delete_task_files", "jpn_title", "download_range", "download_timeout")}
        cfg.update(cfg_dict)
        if cfg['download_ori'] and not self.has_login:
            self.logger.warning(i18n.XEH_DOWNLOAD_ORI_NEED_LOGIN)
        t = Task(url, cfg)
        if t.guid in self._all_tasks:
            if self._all_tasks[t.guid].state in (TASK_STATE_FINISHED, TASK_STATE_FAILED):
                self.logger.debug(i18n.TASK_PUT_INTO_WAIT % t.guid)
                self._all_tasks[t.guid].state = TASK_STATE_WAITING
                self._all_tasks[t.guid].cleanup()
            return 0, t.guid
        self._all_tasks[t.guid] = t
        if not re.match("^%s/[^/]+/\d+/[^/]+/*#*$" % RESTR_SITE, url):
            t.set_fail(ERR_URL_NOT_RECOGNIZED)
        elif not self.has_login and re.match("^https*://exhentai\.org", url):
            t.set_fail(ERR_CANT_DOWNLOAD_EXH)
        else:
            self.tasks.put(t.guid)
            return 0, t.guid
        self.logger.error(i18n.TASK_ERROR % (t.guid, i18n.c(t.failcode)))
        return t.failcode, None

    def del_task(self, guid):
        if guid not in self._all_tasks:
            return ERR_TASK_NOT_FOUND, None
        if TASK_STATE_PAUSED< self._all_tasks[guid].state < TASK_STATE_FINISHED:
            return ERR_DELETE_RUNNING_TASK, None
        self._all_tasks[guid].cleanup(before_delete=True)
        del self._all_tasks[guid]
        return ERR_NO_ERROR, ""

    def pause_task(self, guid):
        if guid not in self._all_tasks:
            return ERR_TASK_NOT_FOUND, None
        t = self._all_tasks[guid]
        if t.state in (TASK_STATE_PAUSED, TASK_STATE_FINISHED, TASK_STATE_FAILED):
            return ERR_TASK_CANNOT_PAUSE, None
        if t._monitor:
            t._monitor._exit = lambda x: True
        t.state = TASK_STATE_PAUSED
        return ERR_NO_ERROR, ""

    def resume_task(self, guid):
        if guid not in self._all_tasks:
            return ERR_TASK_NOT_FOUND, None
        t = self._all_tasks[guid]
        if TASK_STATE_PAUSED< t.state < TASK_STATE_FINISHED:
            return ERR_TASK_CANNOT_RESUME, None
        t.state = max(t.state, TASK_STATE_WAITING)

        self.tasks.put(guid)
        return ERR_NO_ERROR, ""

    def _do_task(self, task_guid):
        task = self._all_tasks[task_guid]
        if task.state == TASK_STATE_WAITING:
            task.state = TASK_STATE_GET_META
        req = self._get_httpreq(util.get_proxy_policy(task.config))
        if not task.page_q:
            task.page_q = Queue() # per image page queue
        if not task.img_q:
            task.img_q = Queue() # (image url, savepath) queue
        monitor_started = False
        while self._exit < XEH_STATE_FULL_EXIT:
            # wait for threads from former task to stop
            if self._all_threads[task.state]:
                self.logger.verbose("wait %d threads in state %s" % (
                    len(self._all_threads[task.state]), task.state))
                for t in self._all_threads[task.state]:
                    t.join()
                self._all_threads[task.state] = []
                # check again before we bring up new threads
                continue
            if task.state >= TASK_STATE_SCAN_IMG and not monitor_started:
                self.logger.verbose("state %d >= %d, bring up montior" % (task.state, TASK_STATE_SCAN_IMG))
                # bring up the monitor here, ahead of workers
                mon = Monitor(req, self.proxy, self.logger, task, ignored_errors=task.config['ignored_errors'])
                _ = ['down-%d' % (i + 1) for i in range(task.config['download_thread_cnt'])]
                # if we jumpstart from a saved session to DOQNLOAD
                # there will be no scan_thread
                # if task.state >= TASK_STATE_SCAN_PAGE:
                #    _ += ['list-1']
                if task.state >= TASK_STATE_SCAN_IMG:
                    _ += ['scan-%d' % (i + 1) for i in range(task.config['scan_thread_cnt'])]
                mon.set_vote_ns(_)
                self._monitor = mon
                task._monitor = mon
                mon.start()
                # put in the lowest state
                self._all_threads[TASK_STATE_SCAN_IMG].append(mon)
                monitor_started = True

            if task.state == TASK_STATE_GET_META: # grab meta data
                try:
                    r = req.request("GET", task.url,
                        filters.flt_metadata,
                        lambda x:task.update_meta(x),
                        lambda x:task.set_fail(x))
                except Exception as ex:
                    self.logger.error(i18n.TASK_ERROR % (task.guid, traceback.format_exc()))
                    task.state = TASK_STATE_FAILED
                    break
                if task.failcode in (ERR_ONLY_VISIBLE_EXH, ERR_GALLERY_REMOVED) and self.has_login and \
                        task.migrate_exhentai():
                    self.logger.info(i18n.TASK_MIGRATE_EXH % task_guid)
                    self.tasks.put(task_guid)
                    break
                elif task.failcode == ERR_IP_BANNED:
                    self.logger.error(i18n.c(ERR_IP_BANNED) % r)
                    task.state = TASK_STATE_FAILED
                    break

            # elif task.state == TASK_STATE_GET_HATHDL: # download hathdl
            #     r = req.request("GET",
            #         "%s/hathdler.php?gid=%s&t=%s" % (task.base_url(), task.gid, task.sethash),
            #         filters.flt_hathdl,
            #         lambda x:(task.meta.update(x),
            #             task.guess_ori(),
            #             task.scan_downloaded()),
            #                 #task.meta['has_ori'] and task.config['download_ori'])),
            #         lambda x:task.set_fail(x),)
            #     self.logger.info(i18n.TASK_WILL_DOWNLOAD_CNT % (
            #         task_guid, task.meta['total'] - len(task._flist_done),
            #         task.meta['total']))
            elif task.state == TASK_STATE_SCAN_PAGE:
                # if task.config['fast_scan'] and not task.has_ori:
                #     self.logger.info(i18n.TASK_FAST_SCAN % task.guid)
                #     for p in task.meta['filelist']:
                #         task.queue_wrapper(task.page_q.put, pichash = p)
                # else:
                # scan by our own, should not be here currently
                # start backup thread
                task.scan_downloaded()
                if task.state == TASK_STATE_FINISHED:
                    continue
                for x in range(0,
                    int(math.ceil(1.0 * task.meta['total'] / int(task.meta['thumbnail_cnt'])))):
                    r = req.request("GET",
                        "%s/?p=%d" % (task.url, x),
                        filters.flt_pageurl,
                        lambda x: task.queue_wrapper(task.page_q.put, url = x),
                        lambda x: task.set_fail(x))
                    if task.failcode:
                        break
            elif task.state == TASK_STATE_SCAN_IMG:
                # print here so that see it after we can join former threads
                self.logger.info(i18n.TASK_TITLE % (
                    task_guid, task.meta['title']))
                self.logger.info(i18n.TASK_WILL_DOWNLOAD_CNT % (
                    task_guid, task.meta['total'] - task.meta['finished'],
                    task.meta['total']))
                # spawn thread to scan images
                for i in range(task.config['scan_thread_cnt']):
                    tid = 'scan-%d' % (i + 1)
                    _ = self._get_httpworker(tid, task.page_q,
                        filters.flt_imgurl_wrapper(task.config['download_ori'] and self.has_login),
                        lambda x, tid = tid: (task.set_reload_url(x[0], x[1], x[2]),
                            task.img_q.put(x[0]),
                            mon.vote(tid, 0)),
                        lambda x, tid = tid: (mon.vote(tid, x[0])),
                        mon.wrk_keepalive,
                        util.get_proxy_policy(task.config),
                        10,
                        False)
                        # we don't need proxy_image in the scan thread
                        # we use default timeout in the scan thread
                    # _._exit = lambda t: t._finish_queue()
                    self._all_threads[TASK_STATE_SCAN_IMG].append(_)
                    _.start()
                task.state = TASK_STATE_DOWNLOAD - 1
            elif task.state == TASK_STATE_SCAN_ARCHIVE:
                task.state = TASK_STATE_DOWNLOAD - 1
            elif task.state == TASK_STATE_DOWNLOAD:
                # spawn thread to download all urls
                for i in range(task.config['download_thread_cnt']):
                    tid = 'down-%d' % (i + 1)
                    _ = self._get_httpworker(tid, task.img_q,
                        filters.download_file_wrapper(task.config['dir']),
                        lambda x, tid = tid: (task.save_file(x[1], x[2], x[0]) and \
                            (self.logger.debug(i18n.XEH_FILE_DOWNLOADED.format(tid, *task.get_fname(x[1]))),
                                mon.vote(tid, 0))),
                        lambda x, tid = tid: (
                            task.page_q.put(task.get_reload_url(x[1])),# if x[0] != ERR_QUOTA_EXCEEDED else None,
                            task.reload_map.pop(x[1]) if x[1] in task.reload_map else None, # delete old url in reload_map
                            self.logger.debug(i18n.XEH_DOWNLOAD_HAS_ERROR % (tid, i18n.c(x[0]))),
                            mon.vote(tid, x[0])),
                        mon.wrk_keepalive,
                        util.get_proxy_policy(task.config),
                        task.config['download_timeout'],
                        True)
                    self._all_threads[TASK_STATE_DOWNLOAD].append(_)
                    _.start()
                # spawn archiver if we need
                if task.config['make_archive']:
                    if self._all_threads[TASK_STATE_MAKE_ARCHIVE]:
                        self._all_threads[TASK_STATE_MAKE_ARCHIVE][0].join()
                        self._all_threads[TASK_STATE_MAKE_ARCHIVE] = []
                    _a = ArchiveWorker(self.logger, task)
                    self._all_threads[TASK_STATE_MAKE_ARCHIVE].append(_a)
                    _a.start()
                # break current task loop
                break

            if task.failcode:
                self.logger.error(i18n.TASK_ERROR % (task_guid, i18n.c(task.failcode)))
                # wait all threads to finish
                break
            else:
                task.state += 1

    def _task_loop(self):
        task_guid = None
        cnt = 0
        while not self._exit:
            # get a new task
            if cnt == 10:
                self.save_session()
                cnt = 0
            try:
                _ = self.tasks.get(False)
                self.last_task_guid = task_guid
                task_guid = _
            except Empty:
                time.sleep(1)
                cnt += 1
                continue
            else:
                task = self._all_tasks[task_guid]
                if TASK_STATE_PAUSED < task.state < TASK_STATE_FINISHED:
                    self.logger.info(i18n.TASK_START % task_guid)
                    self.save_session()
                    cnt = 0
                    self._do_task(task_guid)
        self.logger.info(i18n.XEH_LOOP_FINISHED)
        self._cleanup()

    def _term_threads(self):
        self._exit = XEH_STATE_FULL_EXIT
        for l in self._all_threads:
            for p in l:
                p._exit = lambda x:True

    def _cleanup(self):
        self._exit = self._exit if self._exit > 0 else XEH_STATE_SOFT_EXIT
        self.save_session()
        self._join_all()
        self.logger.cleanup()
        # let's send a request to rpc server to unblock it
        if self.rpc:
            self.rpc._exit = lambda x:True
            import requests
            try:
                requests.get("http://%s:%s/" % (self.cfg['rpc_interface'], self.cfg['rpc_port']))
            except:
                pass
            self.rpc.join()
        # save it again in case we miss something
        self.save_session()
        self._exit = XEH_STATE_CLEAN

    def _join_all(self):
        for l in self._all_threads:
            for p in l:
                p.join()

    def save_session(self):
        with open("h.json", "w") as f:
            try:
                f.write(json.dumps({
                    'tasks':{} if not self.cfg['save_tasks'] else
                        {k: v.to_dict() for k,v in self._all_tasks.items()},
                    'cookies':self.cookies}))
            except Exception as ex:
                self.logger.warning(i18n.SESSION_WRITE_EXCEPTION % traceback.format_exc())
                return ERR_SAVE_SESSION_FAILED, str(ex)
        return ERR_NO_ERROR, None

    def load_session(self):
        if os.path.exists("h.json"):
            with open("h.json") as f:
                try:
                    j = json.loads(f.read())
                except Exception as ex:
                    self.logger.warning(i18n.SESSION_LOAD_EXCEPTION % traceback.format_exc())
                    return ERR_SAVE_SESSION_FAILED, str(ex)
                else:
                    for _ in j['tasks'].values():
                        _t = Task("", {}).from_dict(_)
                        if 'filelist' in _t.meta:
                            _t.scan_downloaded()
                                #_t.meta['has_ori'] and task.config['download_ori'])
                        # since we don't block on scan_img state, an unempty page_q
                        # indicates we should start from scan_img state,
                        if _t.state == TASK_STATE_DOWNLOAD and _t.page_q:
                            _t.state = TASK_STATE_SCAN_IMG
                        self._all_tasks[_['guid']] = _t
                        self.tasks.put(_['guid'])
                    if self._all_tasks:
                        self.logger.info(i18n.XEH_LOAD_TASKS_CNT % len(self._all_tasks))
                    self.cookies.update(j['cookies'])
                    if self.cookies:
                        self.headers.update({'Cookie':util.make_cookie(self.cookies)})
                        self.has_login = 'ipb_member_id' in self.cookies and 'ipb_pass_hash' in self.cookies
        _1xcookie = os.path.join(FILEPATH, ".ehentai.cookie")# 1.x cookie file
        if not self.has_login and os.path.exists(_1xcookie):
            with open(_1xcookie) as f:
                try:
                    cid, cpw = f.read().strip().split(",")
                    self.cookies.update({'ipb_member_id':cid, 'ipb_pass_hash':cpw})
                    self.headers.update({'Cookie':util.make_cookie(self.cookies)})
                    self.has_login = True
                    self.logger.info(i18n.XEH_LOAD_OLD_COOKIE)
                except:
                    pass

        return ERR_NO_ERROR, None

    def login_exhentai(self, name, pwd):
        if 'ipb_member_id' in self.cookies and 'ipb_pass_hash' in self.cookies:
            return
        self.logger.debug(i18n.XEH_LOGIN_EXHENTAI)
        logindata = {
            'UserName':name,
            'returntype':'8',
            'CookieDate':'1',
            'b':'d',
            'bt':'pone',
            'PassWord':pwd
        }
        req = self._get_httpreq(util.get_proxy_policy(self.cfg))
        req.request("POST", "https://forums.e-hentai.org/index.php?act=Login&CODE=01",
            filters.login_exhentai,
            lambda x:(
                setattr(self, 'cookies', x),
                setattr(self, 'has_login', True),
                self.headers.update({'Cookie':util.make_cookie(self.cookies)}),
                self.save_session(),
                self.logger.info(i18n.XEH_LOGIN_OK)),
            lambda x:(self.logger.warning(str(x)),
                self.logger.info(i18n.XEH_LOGIN_FAILED)),
            logindata)
        return ERR_NO_ERROR, self.has_login

    def set_cookie(self, cookie):
        self.cookies.update(util.parse_cookie(cookie))
        self.headers.update({'Cookie':util.make_cookie(self.cookies)})
        if 'ipb_member_id' in self.cookies and 'ipb_pass_hash' in self.cookies:
            self.has_login = True
        return ERR_NO_ERROR, None


if __name__ == '__main__':
    pass
