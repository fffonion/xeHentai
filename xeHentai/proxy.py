#!/usr/bin/env python
# coding:utf-8
# Contributor:
#      fffonion        <fffonion@gmail.com>

import re
import random
from requests.exceptions import ConnectTimeout, ConnectionError, InvalidSchema
from requests.packages.urllib3.exceptions import ProxySchemeUnknown
from . import util

class PoolException(Exception):
    pass

class Pool(object):
    def __init__(self, disable_policy = None):
        self.proxies = {}
        self.errors = {}
        if not disable_policy:
            self.disable_policy = lambda x, y: y >= 5
        else:
            self.disable_policy = disable_policy
        self.disabled = set()

    def proxied_request(self, session):
        l = [i for i in self.proxies.keys() if i not in self.disabled]
        if not l:
            raise PoolException("try to use proxy but no proxies avaliable")
        # _ = self.proxies[random.choice(l)]
        _ = self.proxies[l[0]]
        return _[0](session)

    def has_available_proxies(self):
        return len([i for i in self.proxies.keys() if i not in self.disabled]) == 0

    def trace_proxy(self, addr, weight = 1, check_func = None, exceptions = []):
        def _(func):
            def __(*args, **kwargs):
                ex = None
                try:
                    r = func(*args, **kwargs)
                except Exception as ex:
                    for e in [ConnectTimeout, ConnectionError] + exceptions:
                        if isinstance(ex, e):
                            self.proxies[addr][2] += weight
                            break
                else:
                    if check_func and not check_func(r):
                        self.proxies[addr][2] += weight
                    else:
                        # suc count + 1
                        self.proxies[addr][1] += weight
                if self.disable_policy(*self.proxies[addr][1:]):
                    # add to disabled set
                    self.disabled.add(addr)
                # print(self.proxies[addr])
                if ex:
                    import traceback
                    traceback.print_exc()
                    raise ex
                return r
            return __
        return _

    def add_proxy(self, addr):
        if re.match("socks[45]a*://([^:^/]+)(\:\d{1,5})*/*$", addr):
            p = socks_proxy(addr, self.trace_proxy)
        elif re.match("https*://([^:^/]+)(\:\d{1,5})*/*$", addr):
            p = http_proxy(addr, self.trace_proxy)
        elif re.match("https*://([^:^/]+)(\:\d{1,5})*/.+\.php\?.*b=.+", addr):
            p = glype_proxy(addr, self.trace_proxy)
        else:
            raise ValueError(" %s is not an acceptable proxy address" % addr)
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
    def handle(session):
        import urllib
        argname = re.findall('[&\?]([a-zA-Z\._]+)=[^\d]*', addr)[0]
        bval = re.findall('[&\?]b=(\d*)', addr)
        bval = bval[0] if bval else '4'
        baseurl = re.findall('(https*://[^/]+/.*?\.php)', addr)[0]
        def mkurl(url):
            return "%s?%s=%s&b=%s&f=norefer" % (
                baseurl, argname, urllib.quote_plus(url),
                bval)
        @trace_proxy(addr)
        def f(*args, **kwargs):
            # change url
            url = args[1]
            args = (args[0], mkurl(url),)
            if 'headers' not in kwargs:
                kwargs['headers'] = {}
            kwargs['headers'] = dict(kwargs['headers'])
            # anti hotlinking
            kwargs['headers'].update({'Referer':baseurl})
            _kw_lower = {k.lower():v for k,v in kwargs['headers'].iteritems()}
            if 'cookie' in _kw_lower:
                site = re.findall('https*://([^/]+)/*', url)[0]
                _coo_new = {}
                _coo_old = util.parse_cookie(_kw_lower['cookie'])
                for k in _coo_old:
                    _coo_new["c[%s][/][%s]" % (site, k)] = _coo_old[k]
                kwargs['headers'].update({'Cookie':_coo_new})
            while True:
                rt = session.request(*args, **kwargs)
                if '<input type="hidden" name="action" value="sslagree">' not in rt.text:
                    break
                rt = session.request("GET", "%s/include/process.php?action=sslagree" % baseurl)
                print("retry", rt.headers)
            if rt.headers.get('set-cookie'):
                coo = util.parse_cookie(rt.headers.get('set-cookie').replace(",", ";"))
                for k in coo.keys():
                    _ = re.findall('c\[[^]]+\]\[[^]]+\]\[([^]]+)\]', k)
                    if _:
                        coo[_[0]] = coo[k]
                rt.headers['set-cookie'] = util.make_cookie(coo)
            rt.url = url
            # change cookie domain
            return rt

        return f
    return handle

if __name__ == '__main__':
    import requests
    p = Pool()
    p.add_proxy("sock5://127.0.0.1:16961")
    print(p.proxied_request(requests.Session())("GET", "http://ipip.tk", headers = {}, timeout = 2).headers)
