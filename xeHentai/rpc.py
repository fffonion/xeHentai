#!/usr/bin/env python
# coding:utf-8
# Contributor:
#      fffonion        <fffonion@gmail.com>

import re
import time
import json
import zipfile
import traceback
from hashlib import md5
from threading import Thread
import zlib
import requests
import pickle
from .const import *
from .const import __version__
from .i18n import i18n
if PY3K:
    from socketserver import ThreadingMixIn
    from http.server import HTTPServer, BaseHTTPRequestHandler
    from io import IOBase
    from io import BytesIO as StringIO
    from urllib.parse import urlparse
else:
    from SocketServer import ThreadingMixIn
    from BaseHTTPServer import HTTPServer, BaseHTTPRequestHandler
    from cStringIO import StringIO 
    from urlparse import urlparse

cmdre = re.compile("([a-z])([A-Z])")
pathre = re.compile("/(?:jsonrpc|img/|zip/|static/|ui/$)")
staticre = re.compile("/static/")
imgpathre = re.compile("/img/")
zippathre = re.compile("/zip/")

version_str = "xeHentai/%s" % __version__

class RPCServer(Thread):
    def __init__(self, xeH, bind_addr, secret = None, open_browser = True, logger = None, exit_check = None):
        Thread.__init__(self, name = "rpc")
        Thread.setDaemon(self, True)
        self.xeH = xeH
        self.bind_addr = bind_addr
        self.secret = secret
        self.logger = logger
        self.server = None
        self.open_browser = open_browser
        self._exit = exit_check if exit_check else lambda x:False

    def run(self):
        try:
            self.server = ThreadedHTTPServer(self.bind_addr, lambda *x: Handler(self.xeH, self.secret, *x))
        except Exception as ex:
            self.logger.error(i18n.RPC_CANNOT_BIND % traceback.format_exc())
        else:
            self.logger.info(i18n.RPC_STARTED % (self.bind_addr[0], self.bind_addr[1]))
            url = "http://%s:%s/ui/#host=%s,port=%s,https=no" % (
                self.bind_addr[0], self.bind_addr[1],
                self.bind_addr[0], self.bind_addr[1]
            )
            if self.secret:
                url = url + ",token=" + self.secret
            if self.open_browser:
                import webbrowser
                webbrowser.open(url)
            else:
                self.logger.info(i18n.RPC_WEBUI_PATH % url)
            while not self._exit("rpc"):
                self.server.handle_request()

def is_readable_obj(obj):
    return hasattr(obj, "read")

def is_str_obj(obj):
    if PY3K:
        return isinstance(obj, str)
    return isinstance(obj, basestring)

def hash_link(secret, url):
    _ = "%s-xehentai-%s" % (secret if secret else "", url)
    if PY3K:
        _ = _.encode('utf-8')
    return md5(_).hexdigest()[:8]

def gen_thumbnail(fh, args):
    # returns a new file handler if resized
    # and a boolean indicates there'e error
    try:
        import PIL.Image as Image
    except:
        return fh, True
    if 'w' not in args and 'h' not in args:
        return fh, False
    size = (int(args['w']) if 'w' in args else int(args['h']),
            int(args['h']) if 'h' in args else int(args['w']))
    if not is_readable_obj(fh):
        fh = StringIO(fh)
    with Image.open(fh) as img:
        img.thumbnail(size)
        ret_fh = StringIO()
        img.save(ret_fh, format=img.format)
        ret = ret_fh.getvalue()
        ret_fh.close()
        fh.close()
        return ret, False
    
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

def load_cache():
    if os.path.exists(STATIC_CACHE_FILE):
        try:
            with open(STATIC_CACHE_FILE, "rb") as f:
                r = zlib.decompress(f.read())
                r = pickle.loads(r)
                if 'v' in r or r['v'] == STATIC_CACHE_VERSION:
                    return r
        except:
            pass
    return { "v": STATIC_CACHE_VERSION }

def save_cache(static_cache):
    r = pickle.dumps(static_cache)
    r = zlib.compress(r)
    with open(STATIC_CACHE_FILE, "wb") as f:
        f.write(r)

static_cache = load_cache()
class Handler(BaseHTTPRequestHandler):

    def __init__(self, xeH, secret, *args):
        self.secret = secret
        self.args = args
        self.xeH = xeHentaiRPCExtended(xeH, secret)
        self.http = requests.Session()
        BaseHTTPRequestHandler.__init__(self, *args)

    def version_string(self):
        return version_str
    
    def serve_file(self, f):
        if hasattr(self.xeH, "_monitor"):
            _task = self.xeH._monitor.task
            # needed to lock between archiver
            _task._f_lock.acquire()
        f.seek(0, os.SEEK_END)
        size = f.tell()
        self.xeH.logger.verbose("GET %s 200 %d %s" % (self.path, size, self.client_address[0]))
        self.send_header("Content-Length", size)
        f.seek(0, os.SEEK_SET)
        self.end_headers()
        while True:
            buf = f.read(51200)
            if not buf:
                break
            self.wfile.write(buf)
        if hasattr(self.xeH, "_monitor"):
            _task._f_lock.release()
        return size

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
        code = 200
        rt = b''
        mime = "text/html"
        path = self.path
        while True:
            if imgpathre.match(path):
                args = dict(q.split("=") for q in urlparse(path).query.split("&") if q)
                _ = urlparse(path).path.split("/")
                if len(_) < 5:
                    code = 400
                    break
                _, _, _hash, guid, fid = _[:5]
                right_hash = hash_link(self.secret, "%s/%s" % (guid, fid))
                if right_hash != _hash:
                    self.xeH.logger.warning("RPC: hash mismatch %s != %s" % (right_hash, _hash))
                    code = 403
                    break
                path, f, mime = self.xeH._get_image_path(guid, fid)
                if not f or not os.path.exists(os.path.join(path, f)):
                    zipf = "%s.zip" % path
                    if not os.path.exists(zipf):
                        self.xeH.logger.warning("RPC: can't find %s" % f)
                        code = 404
                        break
                    else:
                        z = zipfile.ZipFile(zipf)
                        try:
                            rt = z.read(f)
                        except Exception as ex:
                            self.xeH.logger.warning("RPC: can't find %s in zipfile: %s" % (f, ex))
                            code = 404
                            break
                        z.close()
                else:
                    rt = open(os.path.join(path, f), 'rb')
                rt, _error = gen_thumbnail(rt, args)
                if _error:
                    self.xeH.logger.warning("RPC: PIL needed for generating thumbnail")
            elif zippathre.match(path):
                # args = urlparse(_).query
                _ = urlparse(path).path.split("/")
                if len(_) < 5:
                    code = 400
                    break
                _, _, _hash, guid, fname = _[:5]
                fname = fname.split('?')[0]
                right_hash = hash_link(self.secret, "%s" % guid)
                if right_hash != _hash:
                    self.xeH.logger.warning("RPC: hash mismatch %s != %s" % (right_hash, _hash))
                    code = 403
                    break
                f = self.xeH._get_archive_path(guid)
                mime = 'application/zip'
                if not f or not os.path.exists(f):
                    self.xeH.logger.warning("RPC: can't find %s" % f)
                    code = 404
                    break
                rt = open(f, 'rb')
            elif path == "/ui/" or staticre.match(path):
                if path == "/ui/":
                    path = "/"
                while True:
                    cache_rt = None
                    should_clear_cache = False
                    headers = { "User-Agent":  version_str }
                    if path in static_cache:
                        cache_rt, mime, tm, lms = static_cache[path]
                        if PY3K and not isinstance(cache_rt, bytes):
                            cache_rt = bytes(cache_rt, 'ascii')
                        if time.time() - STATIC_CACHE_TTL < tm:
                            rt = StringIO(cache_rt)
                            break
                        should_clear_cache = True
                        headers['If-Modified-Since'] = lms

                    req_start_tm = time.time()
                    r = None
                    try:
                        r = self.http.get("http://xehentai.yooooo.us%s?_=%d" %(path, time.time()),
                            headers=headers, timeout=10)
                    except Exception as ex:
                        self.xeH.logger.warn("error pulling %s from remote server: %s" % (path, err))
                    self.xeH.logger.verbose("%.2fs taken to pull %s from remote server %s bytes" % (
                                            time.time() - req_start_tm, path, r and len(r.content) or 0))
                    if r and r.status_code == 200:
                        rt = StringIO(r.content)
                        mime = r.headers['Content-type']
                        if should_clear_cache:
                            # clear all keys, since the js/css hash may change
                            static_cache.clear()
                        static_cache[path] = [r.content, mime, time.time(), r.headers['Last-Modified']]
                        save_cache(static_cache)
                    elif r and r.status_code == 304:
                        # so this is tricky: if we hit /ui/ first and it's not expired
                        # then all other assets should not expire
                        if path == "/":
                            for k in static_cache:
                                if k != "v":
                                    static_cache[k][2] = time.time()
                            save_cache(static_cache)
                        rt = StringIO(cache_rt)
                    elif cache_rt:
                        self.xeH.logger.warn("serving stale cache %s" % (path))
                        rt = StringIO(cache_rt)
                    else:
                        rt = jsonrpc_resp({"id":None}, error_code = ERR_RPC_INVALID_REQUEST)
                    break
            else:
                # fallback to rpc request
                rt = jsonrpc_resp({"id":None}, error_code = ERR_RPC_INVALID_REQUEST)
                mime = "application/json-rpc"
            break

        self.send_response(code)
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("Content-Type", mime)
        
        if is_readable_obj(rt):
            size = self.serve_file(rt)
            rt.close()
        else:
            self.xeH.logger.verbose("GET %s 200 %d %s" % (self.path, len(rt), self.client_address[0]))
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
        code = 200
        rt = b''
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
                authorized = False
                while True:
                    if len(params[0]) == 0:
                        break
                    secret = params[0][0]
                    if not PY3K and isinstance(secret, unicode):
                        secret = secret.encode('utf-8')
                    if is_str_obj(secret) and re.findall("token:%s" % self.secret, secret):
                        params[0].pop(0)
                        authorized = True
                    break
                if not authorized:
                    code = 403
                    rt = jsonrpc_resp({"id":j['id']}, error_code = ERR_RPC_UNAUTHORIZED)
                    break
            self.xeH.logger.verbose("RPC from: %s, cmd: %s, params: %s" % (self.client_address[0], cmd, params))
            try:
                # pop out token if extra token is found
                if len(params[0]) > 0 and 'token:' in params[0][0]:
                    del params[0][0]
                cmd_rt = getattr(self.xeH, cmd_r)(*params[0], **params[1])
            except (ValueError, TypeError) as ex:
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

# extend xeHentai class for rpc commands
class xeHentaiRPCExtended(object):
    def __init__(self, xeH, secret):
        self.xeH = xeH
        self.secret = secret
    
    def get_info(self):
        ret = {"version": self.verstr,
            "threads_zombie": 0, "threads_running": 0,
            "queue_pending": 0, "queue_finished": 0,
            "download_speed": 0,
        }
        if hasattr(self, '_monitor'):
            ret['threads_running'] = len(self._monitor.thread_last_seen)
            ret['threads_zombie'] = len(self._monitor.thread_zombie)
            if self._monitor.task.state > TASK_STATE_PAUSED and self._monitor.task.img_q:
                ret['queue_pending'] = self._monitor.task.img_q.qsize()
                ret['queue_finished'] = self._monitor.task.meta['finished']
                ret['download_speed'] = self._monitor.download_speed
            else:
                ret['queue_pending'] = 0
                ret['queue_finished'] = 0
        return ERR_NO_ERROR, ret
    
    def get_config(self):
        rt = {k: v for k, v in self.cfg.items() if not k.startswith('rpc_') and k not in ('urls',)}
        return ERR_NO_ERROR, rt
    
    def update_config(self, **cfg_dict):
        cfg_dict = {k: v for k, v in cfg_dict.items() if not k.startswith('rpc_') and k not in ('urls',)}
        if 'proxy' in cfg_dict:
            self.xeH.update_config(**cfg_dict)
        return self.get_config()
           
    def list_tasks(self, level = "download"):
        reverse_mode = False
        if level.startswith('!'):
            reverse_mode = True
            level = level[1:]
        level = "TASK_STATE_%s" % level.upper()
        if level not in globals():
            return ERR_TASK_LEVEL_UNDEF, None
        lv = globals()[level]
        rt = [{_k:_v for _k, _v in v.to_dict().items() if _k not in
            ('reload_map', 'duplicate_map', 'renamed_map', 'logger', 'img_q', 'page_q')}
                 for _, v in self._all_tasks.items() if 
                    (reverse_mode and v.state != lv) or (not reverse_mode and v.state == lv)]
        return ERR_NO_ERROR, rt
    
    def _get_image_path(self, guid, fid):
        mime_map = {
            "jpg": "image/jpeg",
            "jepg": "image/jpeg",
            "png": "image/png",
            "gif": "image/gif",
            "bmp": "image/bmp",
            "webp": "image/webp"
        }
        if guid not in self._all_tasks:
            return None, None, None
        t = self._all_tasks[guid]
        fid = str(fid)
        if fid in t.renamed_map:
            f = t.renamed_map[fid]
        else:
            f = t.get_fidpad(fid)

        ext = os.path.splitext(f)[1].lower()[1:]
        if ext not in mime_map:
            mime = "application/octet-stream"
        else:
            mime = mime_map[ext]
        return t.get_fpath(), f, mime
    
    def _get_archive_path(self, guid):
        if guid not in self._all_tasks:
            return None, None
        t = self._all_tasks[guid]
        st = time.time()
        pth = t.make_archive(False)
        et = time.time()
        if et - st > 0.1:
            self.logger.warning('RPC: %.2fs taken to get archive' % (et - st))
        return pth
    
    def get_image(self, guid, request_range=None):
        if guid not in self._all_tasks:
            return ERR_TASK_NOT_FOUND, None
        t = self._all_tasks[guid]
        start = 1
        end = t.meta['total'] + 1
        if request_range:
            request_range = str(request_range)
            _ = request_range.split(',')
            if len(_) == 1:
                start = int(request_range)
            else:
                start = int(_[0])
            end = int(_[0]) + 1
        rt = []
        for fid in range(start, end):
            if fid in t.renamed_map:
                f = t.renamed_map[fid]
            else:
                f = t.get_fidpad(fid)
            uri = "%s/%s" % (t.guid, fid)
            rt.append('/img/%s/%s/%s' % (hash_link(self.secret, uri), uri, f))
        return ERR_NO_ERROR, rt

    
    def __getattr__(self, k):
        # fallback attribute handler
        return getattr(self.xeH, k)

class ThreadedHTTPServer(ThreadingMixIn, HTTPServer):
    """Handle requests in a separate thread."""
    pass
