# coding:utf-8

from ..const import *

err_msg = {
    ERR_URL_NOT_RECOGNIZED: "网址不够绅士",
    ERR_CANT_DOWNLOAD_EXH: "需要登录后才能下载里站",
    ERR_ONLY_VISIBLE_EXH: "这个本子只有里站能看到",
    ERR_MALFORMED_HATHDL: "hathdl文件有猫饼，解析失败",
    ERR_GALLERY_REMOVED: "这个本子被移除了，大概里站能看到",
    ERR_KEY_EXPIRED: "下载链接不太正常",
    ERR_NO_PAGEURL_FOUND: "没有找到页面链接，网站改版了嘛？",
    ERR_CONNECTION_ERROR: "连接有问题？",
    ERR_IP_BANNED: "IP被ban了, 恢复时间: %s",
    ERR_IMAGE_BROKEN: "下载的图片有猫饼",
    ERR_QUOTA_EXCEEDED: "配额超限",
    ERR_TASK_NOT_FOUND: "没有该GUID对应的任务",
    ERR_TASK_LEVEL_UNDEF: "任务过滤等级不存在",
    ERR_DELETE_RUNNING_TASK: "无法删除运行中的任务",
    ERR_TASK_CANNOT_PAUSE: "这个任务无法被暂停",
    ERR_TASK_CANNOT_RESUME: "这个任务无法被恢复",
    ERR_CANNOT_CREATE_DIR: "无法创建文件夹 %s",
    ERR_CANNOT_MAKE_ARCHIVE: "无法制作压缩包 %s",
    ERR_NOT_RANGE_FORMAT: "'%s'不符合范围的格式, 正确的格式为 1-3 或者 5",
#    ERR_HATHDL_NOTFOUND: "hathdl文件未找到"
    ERR_RPC_PARSE_ERROR: "Parse error.",
    ERR_RPC_INVALID_REQUEST: "Invalid request.",
    ERR_RPC_METHOD_NOT_FOUND: "Method not found.",
    ERR_RPC_INVALID_PARAMS: "Invalid method parameter(s).",
    ERR_RPC_UNAUTHORIZED: "Unauthorized",
    ERR_RPC_EXEC_ERROR: "",
    ERR_SAVE_SESSION_FAILED: "",
}

ERR_NOMSG = "未指定的错误，错误号 %d"

XEH_OPT_DESC = "绅♂士下载器"
XEH_OPT_EPILOG = "如果参数未指定，则使用config.py中的默认值; " \
        "讨论和反馈问题：https://yooooo.us/2013/xehentai"
XEH_OPT_URLS = "下载页的网址"
XEH_OPT_u = "用户名"
XEH_OPT_k = "密码"
XEH_OPT_c = "Cookie字符串，如果指定了用户名和密码，此项会被忽略"
XEH_OPT_o = "是否下载原始图片（如果存在），需要登录 (当前: %(default)s)"
XEH_OPT_t = "下载线程数 (当前: %(default)d)"
# XEH_OPT_f = "快速扫描，从hathdl猜测页面链接，但有时会抽风 (当前: %(default)s)"
XEH_OPT_l = "保存日志的路径 (当前: %(default)s)"
XEH_OPT_p = "设置代理, 可以指定多次, 当前支持的类型: socks5/4a, http(s), glype. 代理默认只用于扫描网页 (当前: %(default)s)"
XEH_OPT_proxy_image = "同时使用代理来下载图片和扫描网页（当前: %(default)s)"
XEH_OPT_proxy_image_only = "仅使用代理来下载图片, 不用于扫描网页 (当前: %(default)s)"
XEH_OPT_d = "设置下载目录 (当前: %(default)s)"
XEH_OPT_v = "设置日志装逼等级 (当前: %(default)s)"
XEH_OPT_i = "交互模式，如果开启后台模式，此项会被忽略 (当前: %(default)s)"
XEH_OPT_r = "将图片重命名为原始名称，如果关闭则使用序号 (当前: %(default)s)"
XEH_OPT_daemon = "后台模式 (当前: %(default)s)"
XEH_OPT_rpc_interface = "设置JSON-RPC监听IP (当前: %(default)s)"
XEH_OPT_rpc_port = "设置JSON-RPC监听端口 (当前: %(default)s)"
XEH_OPT_rpc_secret = "设置JSON-RPC密钥 (当前: %(default)s)"
XEH_OPT_a = "下载完成后生成zip压缩包并删除下载目录 (当前: %(default)s)"
XEH_OPT_delete_task_files = "删除任务时同时删除下载的文件 (current: %(default)s)"
XEH_OPT_j = "使用日语标题, 如果关闭则使用英文或罗马字标题 (当前: %(default)s)"
XEH_OPT_download_range = "设置下载的图片范围, 格式为 开始位置-结束位置, 或者单张图片的位置, " \
"使用逗号来分隔多个范围, 例如 5-10,15,20-25, 默认为下载所有"
XEH_OPT_timeout = "设置下载图片的超时 (当前: %(default)s秒)"
XEH_OPT_f = "忽略配额判断，继续下载 (当前: %(current)s)"
XEH_OPT_h = "显示本帮助信息"
XEH_OPT_version = "显示版本信息"
XEH_OPT_IGNORING_I = "后台模式已忽略 -i 参数"


PS_LOGIN = "当前没有登陆，要登陆吗 (y/n)? > "
PS_USERNAME = "输入用户名 > "
PS_PASSWD = "输入密码   > "
PS_URL = "输入地址（使用,分割下载多个）> "
PS_PROXY = "输入代理地址 (可选) > "
PS_DOWNLOAD_ORI = "是否下载原图（默认%s） (y/n)? > "
PS_RENAME_ORI  = "是否自动重命名（默认%s） (y/n)? > "
PS_MAKE_ARCHIVE = "是否制作zip压缩包（默认%s） (y/n)? > "
PS_JPN_TITLE = "是否使用日语标题（默认%s） (y/n)? > "
PS_DOWNLOAD_RANGE = "下载范围, 使用逗号分割多个范围, 回车下载全部 > "
PS_DOWNLOAD_DIR = "下载目录 (当前: %s)\n回车确认或输入新路径 > "

PROXY_CANDIDATE_CNT = "代理池中有%d个代理"

TASK_PUT_INTO_WAIT = "任务 #%s 已存在, 加入等待队列"
TASK_ERROR = "任务 #%s 发生错误: %s"
TASK_MIGRATE_EXH = "任务 #%s 使用里站地址重新下载"
TASK_TITLE = "任务 #%s 标题 %s"
TASK_WILL_DOWNLOAD_CNT = "任务 #%s 将下载%d个文件，共%d个 "
TASK_START = "任务 #%s 开始"
TASK_FINISHED = "任务 #%s 下载完成"
TASK_START_PAGE_RESCAN = "任务 #%s 图片被缩放，进行完整扫描"
# TASK_FAST_SCAN = "任务 #%s 使用快速扫描"
TASK_START_MAKE_ARCHIVE = "任务 #%s 开始打包"
TASK_MAKE_ARCHIVE_FINISHED = "任务 #%s 打包完成，保存在: %s, 用时%.1f秒"
TASK_STOP_QUOTA_EXCEEDED = "任务 #%s 配额超限"
TASK_STUCK = "任务 #%s 卡住了, 可能是脚本有bug, 或者网络连接太慢了"

XEH_STARTED = "xeHentai %s 已启动"
XEH_LOOP_FINISHED = "程序循环已完成"
XEH_LOGIN_EXHENTAI = "登录绅士"
XEH_LOGIN_OK = "已成为绅士"
XEH_LOGIN_FAILED = "无法登录绅士；检查输入是否有误或者换一个帐号。\n推荐在浏览器登录后使用RPC复制cookie到xeHentai (教程: http://t.cn/Rctr4Pf)"
XEH_LOAD_TASKS_CNT = "从存档中读取了%d个任务"
XEH_LOAD_OLD_COOKIE = "从1.x版cookie文件从读取了登录信息"
XEH_DAEMON_START = "后台进程已启动，PID为%d"
XEH_PLATFORM_NO_DAEMON = "后台模式不支持您的系统: %s"
XEH_CLEANUP = "擦干净..."
XEH_CRITICAL_ERROR = "xeHentai 抽风啦:\n%s"
XEH_DOWNLOAD_ORI_NEED_LOGIN = "下载原图需要登录"
XEH_FILE_DOWNLOADED = "绅士-{} 已下载图片 #{} {}"
XEH_RENAME_HAS_ERRORS = "部分图片重命名失败:\n%s"
XEH_DOWNLOAD_HAS_ERROR = "绅士-%s 下载图片时出错: %s, 将在稍后重试"

RPC_STARTED = "RPC服务器监听在 %s:%d"
RPC_TOO_OPEN = "RPC服务器监听在公网IP (%s)，为了安全起见应该设置rpc_secret"
RPC_CANNOT_BIND = "RPC服务器无法启动：%s"

SESSION_LOAD_EXCEPTION = "读取存档时遇到错误: %s"
SESSION_WRITE_EXCEPTION = "写入存档时遇到错误: %s"

THREAD = "绅士"
THREAD_UNCAUGHT_EXCEPTION = "绅士-%s 未捕获的异常\n%s"
THREAD_MAY_BECOME_ZOMBIE = "绅士-%s 可能变成了丧尸"
THREAD_SWEEP_OUT = "绅士-%s 挂了, 不再理它"

QUEUE = "队列"

PROXY_DISABLE_BANNED = "禁用了一个被ban的代理，将在约%s秒后恢复"
