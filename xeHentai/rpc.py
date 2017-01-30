#!/usr/bin/env python
# coding:utf-8
# Contributor:
#      fffonion        <fffonion@gmail.com>

import re
import time
import json
import traceback
from threading import Thread
from .const import *
from .const import __version__
from .i18n import i18n
if PY3K:
    from socketserver import ThreadingMixIn
    from http.server import HTTPServer, BaseHTTPRequestHandler
else:
    from SocketServer import ThreadingMixIn
    from BaseHTTPServer import HTTPServer, BaseHTTPRequestHandler

cmdre = re.compile("([a-z])([A-Z])")
pathre = re.compile("/jsonrpc")

class RPCServer(Thread):
    def __init__(self, xeH, bind_addr, secret = None, logger = None, exit_check = None):
        Thread.__init__(self, name = "rpc")
        Thread.setDaemon(self, True)
        self.xeH = xeH
        self.bind_addr = bind_addr
        self.secret = secret
        self.logger = logger
        self.server = None
        self._exit = exit_check if exit_check else lambda x:False

    def run(self):
        try:
            self.server = ThreadedHTTPServer(self.bind_addr, lambda *x: Handler(self.xeH, self.secret, *x))
        except Exception as ex:
            self.logger.error(i18n.RPC_CANNOT_BIND % traceback.format_exc())
        else:
            self.logger.info(i18n.RPC_STARTED % (self.bind_addr[0], self.bind_addr[1]))
            while not self._exit("rpc"):
                self.server.handle_request()


def jsonrpc_resp(request, ret = None, error_code = None, error_msg = None):
    r = {
        "id":None if not request["id"] else request["id"],
        "jsonrpc":"2.0",
    }
    if error_code:
        r['error'] = {
            'code':error_code,
            "message":i18n.c(error_code) if not error_msg else error_msg
        }
    else:
        r['result'] = ret
    return json.dumps(r)

def path_filter(func):
    def f(self):
        if not pathre.match(self.path):
            self.send_response(404)
            self.send_header("Access-Control-Allow-Origin", "*")
            self.end_headers()
            self.wfile.write(b'\n')
            return
        func(self)
    return f

class Handler(BaseHTTPRequestHandler):

    def __init__(self, xeH, secret, *args):
        self.xeH = xeH
        self.secret = secret
        self.args = args
        BaseHTTPRequestHandler.__init__(self, *args)

    def version_string(self):
        return "xeHentai/%s" % __version__

    def do_OPTIONS(self):
        self.send_response(200)
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("Access-Control-Allow-Headers", "Content-Type")
        self.send_header("Access-Control-Allow-Methods", "POST, GET, OPTIONS")
        self.send_header("Access-Control-Max-Age", "1728000")
        self.end_headers()
        self.wfile.write(b'\n')

    @path_filter
    def do_GET(self):
        rt = jsonrpc_resp({"id":None}, error_code = ERR_RPC_INVALID_REQUEST)
        self.send_response(400)
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("Content-Type", "application/json-rpc")
        self.send_header("Content-Length", len(rt))
        self.end_headers()
        self.wfile.write(rt)
        self.wfile.write(b'\n')
        return

    @path_filter
    def do_POST(self):
        _get_header = lambda h: self.headers.get_all(h)[0] if PY3K else \
            self.headers.getheader(h)
        d = self.rfile.read(int(_get_header('Content-Length')))
        rt = ""
        code = 200
        while True:
            try:
                if PY3K:
                    d = d.decode('utf-8')
                j = json.loads(d)
                assert('method' in j and j['method'] != None and 'id' in j)
            except ValueError:
                code = 400
                rte = jsonrpc_resp({"id":None}, error_code = ERR_RPC_PARSE_ERROR)
                break
            except AssertionError:
                code = 400
                rt = jsonrpc_resp({"id":None}, error_code = ERR_RPC_INVALID_REQUEST)
                break
            cmd = re.findall("xeH\.(.+)", j['method'])
            if not cmd:
                code = 404
                rt = jsonrpc_resp({"id":j['id']}, error_code = ERR_RPC_METHOD_NOT_FOUND)
                break
            # let's make fooBar to foo_bar
            cmd_r = cmdre.sub(lambda m: "%s_%s" % (m.group(1), m.group(2).lower()), cmd[0])
            if not hasattr(self.xeH, cmd_r) or cmd_r.startswith("_"):
                code = 404
                rt = jsonrpc_resp({"id":j['id']}, error_code = ERR_RPC_METHOD_NOT_FOUND)
                break
            params = ([], {}) if 'params' not in j else j['params']
            if self.secret:
                if not PY3K and isinstance(params[0], unicode):
                    params[0] = params[0].encode('utf-8')
                if isinstance(params[0], str) and re.findall("token:%s" % self.secret, params[0]):
                    params.pop(0)
                else:
                    code = 400
                    rt = jsonrpc_resp({"id":j['id']}, error_code = ERR_RPC_UNAUTHORIZED)
                    break
            self.xeH.logger.verbose("RPC from: %s, cmd: %s, params: %s" % (self.client_address[0], cmd, params))
            try:
                cmd_rt = getattr(self.xeH, cmd_r)(*params[0], **params[1])
            except KeyboardInterrupt as ex:
                self.xeH.logger.verbose("RPC exec error:\n%s" % traceback.format_exc())
                code = 500
                rt = jsonrpc_resp({"id":j['id']}, error_code = ERR_RPC_EXEC_ERROR,
                error_msg = str(ex))
                break
            if cmd_rt[0] > 0:
                rt = jsonrpc_resp({"id":j['id']}, error_code = cmd_rt[0], error_msg = cmd_rt[1])
            else:
                rt = jsonrpc_resp({"id":j['id']}, ret = cmd_rt[1])
            break
        self.send_response(code)
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("Content-Type", "application/json-rpc")
        self.send_header("Content-Length", len(rt))
        self.end_headers()
        if PY3K:
            rt = rt.encode('utf-8')
        self.wfile.write(rt)
        self.wfile.write(b'\n')
        return


    def log_message(self, format, *args):
        return

class ThreadedHTTPServer(ThreadingMixIn, HTTPServer):
    """Handle requests in a separate thread."""
    pass
