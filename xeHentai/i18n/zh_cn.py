# coding:utf-8

from ..const import *

err_msg = {
    ERR_URL_NOT_RECOGNIZED: "url not recognized",
    ERR_CANT_DOWNLOAD_EXH: "can't download exhentai.org without login",
    ERR_ONLY_VISIBLE_EXH: "this gallery is only visible in exhentai.org",
    ERR_MALFORMED_HATHDL: "malformed .hathdl, can't parse",
    ERR_GALLERY_REMOVED: "this gallery has been removed, may be visible in exhentai",
    ERR_NO_PAGEURL_FOUND: "no page url found, change of site structure?",
    ERR_TASK_NOT_FOUND: "no such task guid",
    ERR_TASK_LEVEL_UNDEF: "task filter level unknown",
    ERR_DELETE_RUNNING_TASK: "can't delete a running task",
    ERR_TASK_CANNOT_PAUSE: "this task can't be paused",
    ERR_TASK_CANNOT_RESUME: "this task can't be resumed",
    ERR_RPC_PARSE_ERROR: "Parse error.",
    ERR_RPC_INVALID_REQUEST: "Invalid request.",
    ERR_RPC_METHOD_NOT_FOUND: "Method not found.",
    ERR_RPC_INVALID_PARAMS: "Invalid method parameter(s).",
    ERR_RPC_EXEC_ERROR: "",
    ERR_SAVE_SESSION_FAILED: "",
}

ERR_NOMSG = "undefined error message with code %d"

XEH_OPT_DESC = "绅♂士下载器"
XEH_OPT_EPILOG = "如果参数未指定，则使用config.py中的默认值"
XEH_OPT_URLS = "下载页的网址"
XEH_OPT_u = "用户名"
XEH_OPT_k = "密码"
XEH_OPT_c = "Cookie字符串，如果指定了用户名和密码，此项会被忽略"
XEH_OPT_o = "是否下载原始图片（如果存在） (当前: %(default)s)"
XEH_OPT_t = "下载线程数 (当前: %(default)d)"
XEH_OPT_f = "快速扫描，从hathdl猜测页面链接，但有时会抽风 (当前: %(default)s)"
XEH_OPT_l = "保存日志的路径 (当前: %(default)s)"
XEH_OPT_p = "设置代理, 可以指定多次, 当前支持的类型: socks5/4a, http(s) (当前: %(default)s)"
XEH_OPT_d = "设置下载目录 (当前: %(default)s)"
XEH_OPT_v = "设置日志装逼等级 (当前: %(default)s)"
XEH_OPT_i = "交互模式，如果开启后台模式，此项会被忽略 (当前: %(default)s)"
XEH_OPT_daemon = "后台模式 (当前: %(default)s)"
XEH_OPT_rpc_port = "设置JSON-RPC监听IP (当前: %(default)s)"
XEH_OPT_rpc_interface = "设置JSON-RPC监听端口 (当前: %(default)s)"
XEH_OPT_rpc_secret = "设置JSON-RPC密钥 (当前: %(default)s)"


PS_LOGIN = "当前没有登陆，要登陆吗 (y/n)? > "
PS_USERNAME = "输入用户名 > "
PS_PASSWD = "输入密码   > "
PS_URL = "输入地址（使用,分割下载多个）> "
PS_PROXY = "输入代理地址 (可选) > "
PS_DOWNLOAD_ORI = "是否下载原图（默认否） (y/n)? > "
PS_DOWNLOAD_DIR = "下载目录 (当前: %s)\n回车确认或输入新路径 > "

PROXY_CANDIDATE_CNT = "代理池中有%d个代理"

TASK_PUT_INTO_WAIT = "任务 #%s 已存在, 加入等待队列"
TASK_ERROR = "任务 #%s 发生错误: %s"
TASK_MIGRATE_EXH = "任务 #%s 使用里站地址重新下载"
TASK_TITLE = "任务 #%s 标题 %s"
TASK_WILL_DOWNLOAD_CNT = "任务 #%s 将下载%d个文件，共%d个 "
TASK_START = "任务 #%s 开始"
TASK_FINISHED = "任务 #%s 完成"
TASK_START_PAGE_RESCAN = "任务 #%s 图片被缩放，进行完整扫描"
TASK_FAST_SCAN = "任务 #%s 使用快速扫描"

XEH_STARTED = "xeHentai %s 已启动"
XEH_LOOP_FINISHED = "程序循环已完成"
XEH_LOGIN_EXHENTAI = "登录绅士"
XEH_LOGIN_OK = "已成为绅士"
XEH_LOGIN_FAILED = "无法登录绅士"
XEH_LOAD_TASKS_CNT = "从存档中读取了%d个任务"
XEH_DAEMON_START = "后台进程已启动，PID为%d"
XEH_RPC_STARTED = "rpc 服务器监听在 %s:%d"
XEH_PLATFORM_NO_DAEMON = "后台模式不支持您的系统: %s"
XEH_CLEANUP = "擦干净..."
XEH_CRITICAL_ERROR = "xeHentai 抽风啦:\n%s"

SESSION_LOAD_EXCEPTION = "读取存档时遇到错误: %s"
SESSION_WRITE_EXCEPTION = "写入存档时遇到错误: %s"

THREAD = "绅士"
THREAD_UNCAUGHT_EXCEPTION = "绅士-%s 未捕获的异常\n%s"
THREAD_MAY_BECOME_ZOMBIE = "绅士-%s 可能变成了丧尸"
THREAD_SWEEP_OUT = "绅士-%s 挂了, 不再理它"

QUEUE = "队列"
