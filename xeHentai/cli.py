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

from . import config as default_config
sys.path.insert(1, FILEPATH)
try:
    import config
except ImportError:
    config = default_config
sys.path.pop(1)

def start():
    opt = parse_opt()
    xeH = xeHentai()
    if opt.daemon:
        if opt.interactive:
            xeH.logger.warning(i18n.XEH_OPT_IGNORING_I)
        if os.name == "posix":
            pid = os.fork()
            if pid == 0:
                sys.stdin.close()
                sys.stdout = open("/dev/null", "w")
                sys.stderr = open("/dev/null", "w")
                return main(xeH, opt)
        elif os.name == "nt":
            return xeH.logger.error(i18n.XEH_PLATFORM_NO_DAEMON % os.name)
        else:
            return xeH.logger.error(i18n.XEH_PLATFORM_NO_DAEMON % os.name)
        xeH.logger.info(i18n.XEH_DAEMON_START % pid)
    else:
        main(xeH, opt)

def main(xeH, opt):
    xeH.update_config(vars(opt))
    log = xeH.logger
    log.info(i18n.XEH_STARTED % xeH.verstr)
    if opt.cookie:
        xeH.set_cookie(opt.cookie)
    if opt.username and opt.key and not xeH.has_login:
        xeH.login_exhentai(opt.username, opt.key)
    if opt.interactive and not opt.daemon:
        try:
            r = interactive(xeH)
            opt.__dict__.update(r)
            xeH.update_config(r)
        except (KeyboardInterrupt, SystemExit):
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
    -j  --no-jp-name      是否不使用日语命名，默认为否'''

def parse_opt():
    _def = {k:v for k,v in default_config.__dict__.items() if not k.startswith("_")}
    _def.update({k:v for k,v in config.__dict__.items() if not k.startswith("_")})
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
                        action = 'store_true', default = _def['download_ori'],
                        help = i18n.XEH_OPT_o)
    parser.add_argument('-t', '--thread', type = int, metavar = 'N',
                        default = _def['download_thread_cnt'], dest = 'download_thread_cnt',
                        help = i18n.XEH_OPT_t)
    # parser.add_argument('-f', '--fast-scan', action = 'store_true', default = _def.fast_scan,
    #                     help = i18n.XEH_OPT_f)
    parser.add_argument('-d', '--dir', default = os.path.abspath(_def['dir']),
                        help = i18n.XEH_OPT_d)
    parser.add_argument('--daemon', action = 'store_true', default = _def['daemon'],
                        help = i18n.XEH_OPT_daemon)
    parser.add_argument('-l', '--logpath', metavar = '/path/to/eh.log',
                        default = os.path.abspath(_def['log_path']), help = i18n.XEH_OPT_l)
    parser.add_argument('-p', '--proxy', action = 'append', default = _def['proxy'],
                        help = i18n.XEH_OPT_p)
    parser.add_argument('-v', '--verbose', action = 'count', default = _def['log_verbose'],
                        help = i18n.XEH_OPT_v)
    parser.add_argument('--rpc-port', type = int, metavar = "PORT", default = _def['rpc_port'],
                        help = i18n.XEH_OPT_rpc_port)
    parser.add_argument('--rpc-interface', metavar = "ADDR", default = _def['rpc_interface'],
                        help = i18n.XEH_OPT_rpc_interface)
    # parser.add_argument('--rpc-secret', metavar = "...", default = _def['rpc_secret'],
    #                    help = i18n.XEH_OPT_rpc_secret)
    parser.add_argument('-r', '--rename-ori', type = bool, metavar = "BOOL", default = _def['rename_ori'],
                        help = i18n.XEH_OPT_r)

    args = parser.parse_args()
    return args

def interactive(xeH):
    _readline = lambda x:input(logger.safestr(x)) if PY3K else raw_input(logger.safestr(x))

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
    proxy = [proxy] if proxy else xeH.cfg['proxy']
    _dir = _readline(i18n.PS_DOWNLOAD_DIR % os.path.abspath(xeH.cfg['dir']).decode('utf-8')) or xeH.cfg['dir']
    rename_ori = _readline(i18n.PS_RENAME_ORI) == "y"
    return {'urls': url, 'proxy': proxy, 'download_ori': download_ori, 'dir': _dir, 'rename_ori':rename_ori,
            'save_tasks': False}
