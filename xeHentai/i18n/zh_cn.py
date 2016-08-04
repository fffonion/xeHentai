# coding:utf-8

from ..const import *

err_msg = {

}

XEH_OPT_DESC = "绅♂士下载器"
XEH_OPT_EPILOG = "如果参数未指定，则使用config.py中的默认值"
XEH_OPT_URLS = "下载页的网址"
XEH_OPT_u = "用户名"
XEH_OPT_k = "密码"
XEH_OPT_c = "Cookie字符串，如果指定了用户名和密码，此项会被忽略"
XEH_OPT_o = "是否下载原始图片（如果存在） (当前: %(default)s)"
XEH_OPT_t = "下载线程数 (当前: %(default)d)"
XEH_OPT_f = "从hathdl猜测页面链接，可以提高抓取速度，但有时会抽风 (当前: %(default)s)"
XEH_OPT_l = "保存日志的路径 (当前: %(default)s)"
XEH_OPT_p = "设置代理, 当前支持的类型: socks5/4a, http(s) (当前: %(default)s)"
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

XEH_STARTED = "xeHentai %s 已启动"
