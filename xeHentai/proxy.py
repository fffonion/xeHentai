#!/usr/bin/env python
# coding:utf-8
# Contributor:
#      fffonion        <fffonion@gmail.com>

import re
import random
from requests.exceptions import ConnectTimeout, ConnectionError, InvalidSchema
from requests.packages.urllib3.exceptions import ProxySchemeUnknown

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
                    raise ex
                return r
            return __
        return _

    def add_proxy(self, addr):
        if re.match("socks[45]a*://([^:]+:[^@]+@)*(\d{1,3}\.){3}\d{1,3}\:\d{1,5}", addr):
            p = socks_proxy(addr, self.trace_proxy)
        elif re.match("https*://([^:]+:[^@]+@)*(\d{1,3}\.){3}\d{1,3}(\:\d{1,5})*", addr):
            p = http_proxy(addr, self.trace_proxy)
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


if __name__ == '__main__':
    import requests
    p = Pool()
    p.add_proxy("socks5://127.0.0.1:16963")
    print(p.proxied_request(requests.Session())("GET", "http://ipip.tk", timeout = 2).text)
