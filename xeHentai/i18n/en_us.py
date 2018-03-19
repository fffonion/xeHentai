# coding:utf-8

from ..const import *

err_msg = {
    ERR_URL_NOT_RECOGNIZED: "url not recognized",
    ERR_CANT_DOWNLOAD_EXH: "can't download exhentai.org without login",
    ERR_ONLY_VISIBLE_EXH: "this gallery is only visible in exhentai.org",
    ERR_MALFORMED_HATHDL: "malformed .hathdl, can't parse",
    ERR_GALLERY_REMOVED: "this gallery has been removed, may be visible in exhentai",
    ERR_KEY_EXPIRED: "image url is expired",
    ERR_NO_PAGEURL_FOUND: "no page url found, change of site structure?",
    ERR_CONNECTION_ERROR: "a connection problem occurs",
    ERR_IP_BANNED: "IP has been banned, retry in %s",
    ERR_IMAGE_BROKEN: "downloaded image is broken",
    ERR_QUOTA_EXCEEDED: "quota exceeded",
    ERR_TASK_NOT_FOUND: "no such task guid",
    ERR_TASK_LEVEL_UNDEF: "task filter level unknown",
    ERR_DELETE_RUNNING_TASK: "can't delete a running task",
    ERR_TASK_CANNOT_PAUSE: "this task can't be paused",
    ERR_TASK_CANNOT_RESUME: "this task can't be resumed",
    ERR_CANNOT_CREATE_DIR: "can't create directory %s",
    ERR_CANNOT_MAKE_ARCHIVE: "can't make archive %s",
    ERR_NOT_RANGE_FORMAT: "'%s' is not a range format, expecting '1-2' or '3'",
#    ERR_HATHDL_NOTFOUND: "hathdl not found",
    ERR_RPC_PARSE_ERROR: "Parse error.",
    ERR_RPC_INVALID_REQUEST: "Invalid request.",
    ERR_RPC_METHOD_NOT_FOUND: "Method not found.",
    ERR_RPC_INVALID_PARAMS: "Invalid method parameter(s).",
    ERR_RPC_UNAUTHORIZED: "Unauthorized",
    ERR_RPC_EXEC_ERROR: "",
    ERR_SAVE_SESSION_FAILED: "",
}

ERR_NOMSG = "undefined error message with code %d"

XEH_OPT_DESC = "xeHentai Downloader NG"
XEH_OPT_EPILOG = "Values shown as current is read from config.py " \
                "and can be overriden by command line options. " \
                "Discuss and bug reporting at https://yooooo.us/2013/xehentai"
XEH_OPT_URLS = "gallery url(s) to download"
XEH_OPT_u = "username"
XEH_OPT_k = "password"
XEH_OPT_c = "cookie string, will be overriden if given -u and -k"
XEH_OPT_o = "download original images, needs to login (current: %(default)s)"
XEH_OPT_t = "download threads count (current: %(default)d)"
# XEH_OPT_f = "fast scan, guess page url from .hathdl file, not working everytime (current: %(default)s)"
XEH_OPT_l = "define log path (current: %(default)s)"
XEH_OPT_p = "set download proxies, can be used multiple times, currenlty supported: socks5/4a, http(s), glype. " \
"Proxies are only used on webpages by default (current: %(default)s)"
XEH_OPT_proxy_image = "use proxies on images and webpages (current: %(default)s)"
XEH_OPT_proxy_image_only = "only use proxies on images, not webpages (current: %(default)s)"
XEH_OPT_d = "set download directory (current: %(default)s)"
XEH_OPT_v = "show more detailed log (current: %(default)s)"
XEH_OPT_i = "interactive mode, will be ignored in daemon mode (current: %(default)s)"
XEH_OPT_r = "rename gallery image to original name, use sequence name if turned off  (current: %(default)s)"
XEH_OPT_daemon = "daemon mode, can't use with -i (current: %(default)s)"
XEH_OPT_rpc_interface = "bind jsonrpc server to this address (current: %(default)s)"
XEH_OPT_rpc_port = "bind jsonrpc server to this port (current: %(default)s)"
XEH_OPT_rpc_secret = "jsonrpc secret string (current: %(default)s)"
XEH_OPT_a = "make an archive (.zip) after download and delete directory (current: %(default)s)"
XEH_OPT_delete_task_files = "delete downloaded files when deleting a task (current: %(default)s)"
XEH_OPT_j = "use Japanese title, use English/Romaji title if turned off (current: %(default)s)"
XEH_OPT_download_range = "specify ranges of images to be downloaded, in format start-end, or single index, " \
"use comma to concat multiple ranges, e.g.: 5-10,15,20-25, default to download all images"
XEH_OPT_timeout = "set image download timeout (current: %(default)ss)"
XEH_OPT_f = "download regardless of quota exceeded warning (current: %(current)s)"
XEH_OPT_h = "show this help message and exit"
XEH_OPT_version = "show program's version number and exit"
XEH_OPT_IGNORING_I = "ignoring -i option in daemon mode"

PS_LOGIN = "login to exhentai (y/n)? > "
PS_USERNAME = "Username > "
PS_PASSWD = "Password > "
PS_URL = "URL (seperate with ,)> "
PS_PROXY = "Proxy (optional) > "
PS_DOWNLOAD_ORI = "Download original (y/n, default:%s)? > "
PS_RENAME_ORI = "Rename to original name (y/n, default:%s)? > "
PS_MAKE_ARCHIVE = "Make archive (y/n, default:%s)? > "
PS_JPN_TITLE = "Use Japanese title (y/n, default:%s)? > "
PS_DOWNLOAD_RANGE = "Download range, press enter to download all > "
PS_DOWNLOAD_DIR = "Download to (default: %s)\npress enter or enter new > "

PROXY_CANDIDATE_CNT = "proxy pool has %d candidates"

TASK_PUT_INTO_WAIT = "task #%s already exists, put into waiting state"
TASK_ERROR = "task #%s error: %s"
TASK_MIGRATE_EXH = "task #%s migrate to exhentai.org"
TASK_TITLE = "task #%s title %s"
TASK_WILL_DOWNLOAD_CNT = "task #%s will download %d/%d files"
TASK_START = "task #%s start"
TASK_FINISHED = "task #%s download finishd"
TASK_START_PAGE_RESCAN = "task #%s resample detected, start full scan"
# TASK_FAST_SCAN = "task #%s uses fast scan"
TASK_START_MAKE_ARCHIVE = "task #%s start making archive"
TASK_MAKE_ARCHIVE_FINISHED = "task #%s archive saved at: %s, use %.1fs"
TASK_STOP_QUOTA_EXCEEDED = "task #%s quota exceeded"
TASK_STUCK = "task #%s is stuck, there may be some bugs in xeHentai, or the connection is too slow"

XEH_STARTED = "xeHentai %s started."
XEH_LOOP_FINISHED = "application task loop finished"
XEH_LOGIN_EXHENTAI = "login exhentai"
XEH_LOGIN_OK = "login exhentai successfully"
XEH_LOGIN_FAILED = "can't login exhentai, check your credentials or try another account.\nIt's recommended to login in browser and use RPC to transfer cookie to xeHentai (see http://t.cn/Rctr4Pf)"
XEH_LOAD_TASKS_CNT = "load %d tasks from saved session"
XEH_LOAD_OLD_COOKIE = "load cookie from legacy cookie file"
XEH_DAEMON_START = "daemon start at PID %d"
XEH_PLATFORM_NO_DAEMON = "daemon mode is not supported on platform: %s"
XEH_CLEANUP = "cleaning up..."
XEH_CRITICAL_ERROR = "xeHentai throws critical error:\n%s"
XEH_DOWNLOAD_ORI_NEED_LOGIN = "haven't login, so I won't download original images"
XEH_FILE_DOWNLOADED = "file downloaded by thread-{} #{} {}"
XEH_RENAME_HAS_ERRORS = "some files are not renamed:\n%s"
XEH_DOWNLOAD_HAS_ERROR = "thread-%s retry because of error: %s"

RPC_STARTED = "RPC server listening on %s:%d"
RPC_TOO_OPEN = "RPC server is listening on public interface (%s) but no rpc_secret defined, which is not safe"
RPC_CANNOT_BIND = "RPC server can't listen on requested address: %s"

SESSION_LOAD_EXCEPTION = "exception occurs when loading saved session: %s"
SESSION_WRITE_EXCEPTION = "exception occurs when writing saved session: %s"

THREAD = "thread"
THREAD_UNCAUGHT_EXCEPTION = "thread-%s uncaught exception\n%s"
THREAD_MAY_BECOME_ZOMBIE = "thread-%s may became zombie"
THREAD_SWEEP_OUT = "thread-%s is dead, deref it"

QUEUE = "queue"

PROXY_DISABLE_BANNED = "disable a banned proxy, expire in about %ss"
