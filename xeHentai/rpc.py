#!/usr/bin/env python
# coding:utf-8
# Contributor:
#      fffonion        <fffonion@gmail.com>

import re
import time
import json
from threading import Thread
from SocketServer import ThreadingMixIn
from BaseHTTPServer import HTTPServer, BaseHTTPRequestHandler
from .const import *
from .const import __version__
from .i18n import i18n

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
        self.server = ThreadedHTTPServer(self.bind_addr, lambda *x: Handler(self.xeH, *x))
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
            self.wfile.write('\n')
            return
        func(self)
    return f

class Handler(BaseHTTPRequestHandler):

    def __init__(self, xeH, *args):
        self.xeH = xeH
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
        self.wfile.write('\n')

    @path_filter
    def do_GET(self):
        rt = jsonrpc_resp({"id":None}, error_code = ERR_RPC_INVALID_REQUEST)
        self.send_response(400)
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("Content-Type", "application/json-rpc")
        self.send_header("Content-Length", len(rt))
        self.end_headers()
        self.wfile.write(rt)
        self.wfile.write('\n')
        return

    @path_filter
    def do_POST(self):
        d = self.rfile.read(int(self.headers.getheader('Content-Length')))
        rt = ""
        code = 200
        while True:
            try:
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
            try:
                params = ([], {}) if 'params' not in j else j['params']
                cmd_rt = getattr(self.xeH, cmd_r)(*params[0], **params[1])
            except ValueError as ex:
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
        self.wfile.write(rt)
        self.wfile.write('\n')
        return


    def log_message(self, format, *args):
        return

class ThreadedHTTPServer(ThreadingMixIn, HTTPServer):
    """Handle requests in a separate thread."""
    pass
