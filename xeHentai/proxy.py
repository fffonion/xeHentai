#!/usr/bin/env python
# coding:utf-8
# Contributor:
#      fffonion        <fffonion@gmail.com>

import re
import time
import random
from requests.exceptions import ConnectTimeout, ConnectionError, ProxyError, InvalidSchema
from requests.packages.urllib3.exceptions import ProxySchemeUnknown
from . import util
from .const import *

MAX_FAIL = 5

class PoolException(Exception):
    pass

class Pool(object):
    def __init__(self, disable_policy = None):
        self.proxies = {}
        self.errors = {}
        if not disable_policy:
            self.disable_policy = lambda x, y: y >= MAX_FAIL
        else:
            self.disable_policy = disable_policy
        self.disabled = {} # key: expire

    def proxied_request(self, session):
        for d in self.disabled:
            if 0 < self.disabled[d] < time.time():
                try:
                    del self.disabled[d]
                except:
                    pass
        l = [i for i in self.proxies.keys() if i not in self.disabled]
        if not l:
            raise PoolException("try to use proxy but no proxies avaliable")
        # _ = self.proxies[random.choice(l)]
        _ = self.proxies[l[0]]
        return _[0](session), self.not_good(l[0])

    def has_available_proxies(self):
        return len([i for i in self.proxies.keys() if i not in self.disabled]) == 0

    def not_good(self, addr):
        def n(weight = MAX_FAIL, expire = 0):
            self.proxies[addr][2] += weight
            if self.disable_policy(*self.proxies[addr][1:]):
                # add to disabled set
                self.disabled[addr] = expire + time.time()
        return n

    def trace_proxy(self, addr, weight = 1, check_func = None, exceptions = []):
        def _(func):
            def __(*args, **kwargs):
                ex = None
                try:
                    r = func(*args, **kwargs)
                except Exception as _ex:
                    ex = _ex
                    for e in [ConnectTimeout, ConnectionError, ProxyError] + exceptions:
                        if isinstance(ex, e):
                            # ignore BadStatusLine, this doesn't mean the proxy is bad
                            if e == ConnectionError and 'BadStatusLine' in str(e):
                                continue
                            self.proxies[addr][2] += weight
                            break
                else:
                    if check_func and not check_func(r):
                        self.proxies[addr][2] += weight
                    else:
                        # suc count + 1
                        self.proxies[addr][1] += weight
                if self.disable_policy(*self.proxies[addr][1:]):
                    # add to disabled set and never expire
                    self.disabled[addr] = 0
                # print(self.proxies[addr])
                if ex:
                    # import traceback
                    # traceback.print_exc()
                    raise ex
                return r
            return __
        return _

    def add_proxy(self, addr):
        if re.match("socks[45][ah]*://([^:^/]+)(\:\d{1,5})*/*$", addr):
            p = socks_proxy(addr, self.trace_proxy)
        elif re.match("https*://([^:^/]+)(\:\d{1,5})*/*$", addr):
            p = http_proxy(addr, self.trace_proxy)
        elif re.match("https*://([^:^/]+)(\:\d{1,5})*/.+\.php\?.*b=.+", addr):
            p = glype_proxy(addr, self.trace_proxy)
        else:
            raise ValueError("%s is not an acceptable proxy address" % addr)
        self.proxies[addr] = [p, 0, 0]

def socks_proxy(addr, trace_proxy):
    proxy_info = {
        'http':addr,
        'https':addr
    }
    def handle(session):
        @trace_proxy(addr, exceptions = [ProxySchemeUnknown, InvalidSchema])
        def f(*args, **kwargs):
            kwargs.update({'proxies': proxy_info})
            return session.request(*args, **kwargs)
        return f
    return handle

def http_proxy(addr, trace_proxy):
    proxy_info = {
        'http':addr,
        'https':addr
    }
    def handle(session):
        @trace_proxy(addr)
        def f(*args, **kwargs):
            kwargs.update({'proxies': proxy_info})
            return session.request(*args, **kwargs)
        return f
    return handle

def glype_proxy(addr, trace_proxy):
    g_session = {"s":""}
    def handle(session, g_session = g_session):
        import urllib
        argname = re.findall('[&\?]([a-zA-Z\._]+)=[^\d]*', addr)[0]
        bval = re.findall('[&\?]b=(\d*)', addr)
        bval = bval[0] if bval else '4'
        server, inst_loc, script = re.findall('(https*://[^/]+)/(.*?)([^/]+\.php)', addr)[0]
        urlre = re.compile('/%s%s\?u=([^&"\']+)&[^"\']+' % (inst_loc, script))
        def mkurl(url):
            return "%s/%s%s?%s=%s&b=%s&f=norefer" % (
                server, inst_loc, script, argname,
                (urllib.parse if PY3K else urllib).quote_plus(url), bval)
        @trace_proxy(addr)
        def f(*args, **kwargs):
            # change url
            url = args[1]
            args = (args[0], mkurl(url),)
            kwargs['headers'] = dict(session.headers)
            # anti hotlinking
            kwargs['headers'].update({'Referer':"%s/%s%s" % (server, inst_loc, script)})
            _coo_new = dict(g_session) if g_session['s'] else {}
            if 'Cookie' in kwargs['headers']:
                site = re.findall('https*://([^/]+)/*', url)[0]
                _coo_old = util.parse_cookie(kwargs['headers']['Cookie'])
                for k in _coo_old:
                    _coo_new["c[%s][/][%s]" % (site, k)] = _coo_old[k]
                kwargs['headers']['Cookie'] = util.make_cookie(_coo_new)
            tried = 0
            while True:
                if tried == 2:
                    raise PoolException("can't bypass glype https warning")
                rt = session.request(*args, **kwargs)
                if '<input type="hidden" name="action" value="sslagree">' not in rt.text:
                    break
                rt = session.request("GET", "%s/%sincludes/process.php?action=sslagree" % (server, inst_loc),
                    allow_redirects = False, **kwargs)
                if rt.headers.get('set-cookie'):
                    _coo_new.update(util.parse_cookie(rt.headers.get('set-cookie').replace(",", ";")))
                    kwargs['headers']['Cookie'] = util.make_cookie(_coo_new)
                    if 's' in _coo_new:
                        g_session["s"] = _coo_new['s']
                        # print(g_session)
                tried += 1

            if rt.headers.get('set-cookie'):
                coo = util.parse_cookie(rt.headers.get('set-cookie').replace(",", ";"))
                for k in list(coo.keys()):
                    _ = re.findall('c\[[^]]+\]\[[^]]+\]\[([^]]+)\]', k)
                    if _:
                        coo[_[0]] = coo[k]
                rt.headers['set-cookie'] = util.make_cookie(coo)
            # change url back, only change on text/* mime types
            rt.url = url
            if rt.headers.get('content-type').startswith("text"):
                if PY3K:
                    rt._content = rt._content.decode('utf-8')
                _ = re.match('<div id="error">(.*?)</div>', rt.content)
                if _:
                    raise PoolException("glype returns: %s" % _[0])
                # change transformed url back
                rt._content = urlre.sub(lambda x:(urllib.parse if PY3K else urllib).unquote(x.group(1)), rt._content)
                if PY3K:
                    rt._content = rt._content.encode('utf-8')
            return rt

        return f
    return handle

if __name__ == '__main__':
    import requests
    p = Pool()
    p.add_proxy("sock5://127.0.0.1:16961")
    print(p.proxied_request(requests.Session())("GET", "http://ipip.tk", headers = {}, timeout = 2).headers)
