#!/usr/bin/env python
# coding:utf-8
# Contributor:
#      fffonion        <fffonion@gmail.com>

from __future__ import absolute_import
import os
import time
import argparse
import traceback
from threading import Thread
from .i18n import i18n
from .core import xeHentai
from .const import *
from .util import logger

sys.path.insert(1, FILEPATH)
try:
    import config
except ImportError:
    from . import config
sys.path.pop(1)

def start():
    opt = parse_opt()
    xeH = xeHentai()
    xeH.update_config(vars(opt))
    if opt.daemon:
        if os.name == "posix":
            pid = os.fork()
            if pid == 0:
                #sys.stdin.close()
                #sys.stdout = open("/dev/null", "w")
                #sys.stderr = open("/dev/null", "w")
                return main(xeH, opt)
        elif os.name == "nt":
            import multiprocessing
            p = multiprocessing.Process(target = main, args = (xeH, opt, ))
            p.start()
            pid = p.pid
        else:
            return xeH.logger.error(i18n.XEH_PLATFORM_NO_DAEMON % os.name)
        xeH.logger.info(i18n.XEH_DAEMON_START % pid)
    else:
        main(xeH, opt)

def main(xeH, opt):
    log = xeH.logger
    log.info(i18n.XEH_STARTED % xeH.verstr)
    if opt.cookie:
        xeH.set_cookie(opt.cookie)
    if opt.username and opt.key and not xeH.has_login:
        xeH.login_exhentai(opt.username, opt.key)
    if opt.interactive:
        try:
            r = interactive(xeH)
            opt.__dict__.update(r)
            xeH.update_config(r)
        except KeyboardInterrupt:
            log.info(i18n.XEH_CLEANUP)
            xeH._cleanup()
            return
    try:
        if opt.urls:
            for u in opt.urls:
                xeH.add_task(u.strip())
            # finished this task and exit xeHentai
            Thread(target = lambda:(time.sleep(0.618), setattr(xeH, "_exit", XEH_STATE_SOFT_EXIT))).start()
        Thread(target = xeH._task_loop, name = "main" ).start()
        while xeH._exit < XEH_STATE_CLEAN:
            time.sleep(1)
    except KeyboardInterrupt:
        log.info(i18n.XEH_CLEANUP)
        xeH._term_threads()
    except Exception as ex:
        log.error(i18n.XEH_CRITICAL_ERROR % traceback.format_exc())
        xeH._term_threads()
    else:
        sys.exit(0) # this is mandatory for single task auto exit
    try:
        # we should call cleanup ourself because we break out of task_loop
        xeH._cleanup()
    except KeyboardInterrupt:
        pass
    # this is mandatory for ctrl+c kill
    os._exit(0)

''' -ro --redirect-norm   是否应用在线代理到已解析到的非原图，默认不启用
    -f  --force           即使超出配额也下载，默认为否
    -re --rename          是否重命名成原始文件名
    -j  --no-jp-name      是否不使用日语命名，默认为否'''

def parse_opt():
    parser = argparse.ArgumentParser(description = i18n.XEH_OPT_DESC, epilog = i18n.XEH_OPT_EPILOG)
    # the followings are handled in cli
    parser.add_argument('-u', '--username', help = i18n.XEH_OPT_u)
    parser.add_argument('-k', '--key', help = i18n.XEH_OPT_k)
    parser.add_argument('-c', '--cookie', help = i18n.XEH_OPT_c)
    parser.add_argument('-i', '--interactive', action = 'store_true', default = False,
                        help = i18n.XEH_OPT_i)
    # the followings are passed to xeHentai
    parser.add_argument('urls', metavar = 'url', type = str, nargs = '*',
                        help = i18n.XEH_OPT_URLS)
    parser.add_argument('-o', '--download-ori',
                        action = 'store_true', default = config.download_ori,
                        help = i18n.XEH_OPT_o)
    parser.add_argument('-t', '--thread', type = int, metavar = 'N',
                        default = config.download_thread_cnt, dest = 'download_thread_cnt',
                        help = i18n.XEH_OPT_t)
    parser.add_argument('-f', '--fast-scan', action = 'store_true', default = config.fast_scan,
                        help = i18n.XEH_OPT_f)
    parser.add_argument('-d', '--dir', default = os.path.abspath(config.dir),
                        help = i18n.XEH_OPT_d)
    parser.add_argument('--daemon', action = 'store_true', default = config.daemon,
                        help = i18n.XEH_OPT_daemon)
    parser.add_argument('-l', '--logpath', metavar = '/path/to/eh.log',
                        default = os.path.abspath(config.log_path), help = i18n.XEH_OPT_l)
    parser.add_argument('-p', '--proxy', action = 'append', default = config.proxy,
                        help = i18n.XEH_OPT_p)
    parser.add_argument('-v', '--verbose', action = 'count', default = config.log_verbose,
                        help = i18n.XEH_OPT_v)
    parser.add_argument('--rpc-port', type = int, metavar = "PORT", default = config.rpc_port,
                        help = i18n.XEH_OPT_rpc_port)
    parser.add_argument('--rpc-interface', metavar = "ADDR", default = config.rpc_interface,
                        help = i18n.XEH_OPT_rpc_interface)
    # parser.add_argument('--rpc-secret', metavar = "...", default = config.rpc_secret,
    #                    help = i18n.XEH_OPT_rpc_secret)

    args = parser.parse_args()
    return args

def interactive(xeH):
    def _readline(s):
        if not isinstance(s, unicode):
            s = s.decode("utf-8")
        return raw_input(logger.convhans(s).encode(locale.getdefaultlocale()[1] or 'utf-8', 'replace'))

    if not xeH.has_login and _readline(i18n.PS_LOGIN) == "y":
        uname = pwd = ""
        while not uname:
            uname = _readline(i18n.PS_USERNAME)
        while not pwd:
            pwd = _readline(i18n.PS_PASSWD)
        xeH.login_exhentai(uname, pwd)
    url = proxy = ""
    while not url:
        url = _readline(i18n.PS_URL)
    url = url.split(",")
    download_ori = _readline(i18n.PS_DOWNLOAD_ORI) == "y"
    proxy = _readline(i18n.PS_PROXY).strip()
    proxy = [proxy] if proxy else None
    _dir = _readline(i18n.PS_DOWNLOAD_DIR % os.path.abspath(xeH.cfg['dir'])) or xeH.cfg['dir']
    return {'urls': url, 'proxy': proxy, 'download_ori': download_ori, 'dir': _dir}
