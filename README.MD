# 绅♂士♂站♂小♂爬♂虫

## 快速入门

windows用户可以下载可执行文件 [这里](https://github.com/fffonion/xeHentai/releases) [或这里](http://dl.yooooo.us/share/xeHentai/)

或者可以运行源码

```shell
pip install -U requests[socks]
git clone https://github.com/fffonion/xeHentai.git
cd xeHentai
python ./setup.py install
xeH
```

新版本默认为命令行模式，如果需要使用交互模式，请运行`xeH.py -i`

## 详细说明

### 配置文件

使用源码运行的用户请先将`xeHentai/config.py`复制到当前目录。

配置的优先级为 交互模式 > 命令行参数 > 用户config.py > 内置config.py。

参数说明：

 - **dir** 下载目录，默认为当前目录
 - **download_ori** 是否下载原图，默认为否
 - **download_thread_cnt** 下载线程数，默认为5
 - **scan_thread_cnt** 扫描线程数，默认为1
 - **proxy** 代理列表，形如`["socks://127.0.0.1:1080", "http://127.0.0.1:8080"]`
 - **log_path** 日志路径，默认为`eh.log`
 - **log_verbose** 日志等级，可选1-3，值越大输出越详细，默认为2
 - **rename_ori** 将图片重命名为原始名称，如果关闭则使用序号，默认为否
 - **daemon** 后台模式，默认为否
 - **fast_scan** 快速扫描，从hathdl猜测页面链接，可以提高抓取速度，但有时会抽风，默认为否
 - **rpc_interface** RPC绑定的IP，默认为`localhost`
 - **rpc_port** RPC绑定的端口，默认为`None`
 - **rpc_secret** RPC密钥，默认为`None`
 - **save_tasks** 是否保存任务到`h.json`，默认为是


### 命令行模式
```
用法: xeH [-h] [-u USERNAME] [-k KEY] [-c COOKIE] [-i] [-o] [-t N] [-f]
           [-d DIR] [--daemon] [-l /path/to/eh.log] [-p PROXY] [-v]
           [--rpc-port PORT] [--rpc-interface ADDR]
           [url [url ...]]

绅♂士下载器

可选参数:
  -h, --help            显示帮助
  -u USERNAME, --username USERNAME
                        用户名
  -k KEY, --key KEY     密码
  -c COOKIE, --cookie COOKIE
                        Cookie字符串，如果指定了用户名和密码，此项会被忽略
  -i, --interactive     交互模式，如果开启后台模式，此项会被忽略 (默认: False)
  -o, --download-ori    是否下载原始图片（如果存在），需要登录 (默认: False)
  -t N, --thread N      下载线程数 (默认: 5)
  -d DIR, --dir DIR     设置下载目录 (默认: 当前目录)
  --daemon              后台模式 (默认: False)
  -l /path/to/eh.log, --logpath /path/to/eh.log
                        保存日志的路径 (默认: /eh.log)
  -p PROXY, --proxy PROXY
                        设置代理, 可以指定多次, 当前支持的类型: socks5/4a, http(s), glype (默认: 无)
  -v, --verbose         设置日志装逼等级 (默认: 2)
  --rpc-port PORT       设置JSON-RPC监听IP (默认: None)
  --rpc-interface ADDR  设置JSON-RPC监听端口 (默认: localhost)
  --rpc-secret ...      设置JSON-RPC密钥 (默认: None)
  -r BOOL, --rename-ori BOOL
                        将图片重命名为原始名称，如果关闭则使用序号 (默认: True)
  -h, --help            显示本帮助信息
  --version             显示版本信息

如果参数未指定，则使用config.py中的默认值

```

如果参数未指定，则使用config.py中的默认值；否则将覆盖config.py设置的值。

### JSON-RPC

在指定`rpc_interface`和`rpc_port`后，xeHentai会启动RPC服务器。典型的请求如下：

```
$ curl localhost:8010/jsonrpc -d '{"jsonrpc": "2.0", "id": 1, "method":"xeH.addTask", "params":[[args],{kwargs}]}'
{"jsonrpc": "2.0", "id": 1, "result": "36df423e"}
```

如果`rpc_secret`设置为**hentai**，则需在params中带上这个值：
```
$ curl localhost:8010/jsonrpc -d '{"jsonrpc": "2.0", "id": 1, "method":"xeH.addTask", "params":["token:hentai",[args],{kwargs}]}'
{"jsonrpc": "2.0", "id": 1, "result": "36df423e"}
```

其中`method`为调用的方法，必须以**xeH.** 开头。在`core.py`的xeHentai类中，所有不以下划线`_`开头的方法均可以通过RPC调用，但需将方法名的下划线命名法改为驼峰命名法。如`add_task`需改为`addTask`。

参数列表请参阅xeHentai类。

如果浏览器安装了用户脚本插件，可以[下载xeHentaiHelper.user.js](http://dl.yooooo.us/userscripts/xeHentaiHelper.user.js)，将会在页面上添加`Add to xeHentai`链接，以支持将当前页面添加到xeHentai中。Chrome用户需要安装[Tampermonkey](https://chrome.google.com/webstore/detail/tampermonkey/dhdgffkkebhmkfjojejmpbldmpobfkfo)，
Firefox用户需要安装[Greasemonkey](https://addons.mozilla.org/en-US/firefox/addon/greasemonkey/)，Opera和傲游用户需要安装暴力猴。

### 运行模式

如果通过命令行或交互模式指定了下载url，xeHentai会会在下载完成后退出。

如果命令行没有指定url，xeHentai将会在完成存档中的队列（如果存在）后继续等待。（待修改）

如果指定了后台模式（`-d`或设置`daemon`为`True`），xeHentai将会在保持后台运行。

### 代理

目前支持三种模式的代理：

 - socks代理，如`socks5://127.0.0.1:1080`。
 - http(s)代理，如`http://127.0.0.1:8080`。
 - glype代理，如`http://example.com/browse.php?u=a&b=4`。请根据实际情况修改`b`的名称。glype是目前使用最广的php在线代理，使用时请取消勾选“加密url(Encrypt URL)”、勾选“允许cookies (Allow Cookies)”后随意打开一个网页，然后把网址粘贴进来

可以指定多个代理，格式如`['socks5://127.0.0.1:1080', 'http://127.0.0.1:8080']`。

## 其他说明

### 配额

直接从服务器及镜像途径下载的图片计入配额，从H@H下载的不计算；下载新发布的、冷门的漫画更有可能消耗配额，下载热门漫画基本不消耗配额

![quota](http://ww3.sinaimg.cn/large/436919cbjw1e314v6gxtzj.jpg)

## License

GPLv3
***
![@fffonion](http://img.t.sinajs.cn/t5/style/images/register/logo.png)[@fffonion](http://weibo.com/376463435)&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;![Blog](https://s.w.org/about/images/logos/wordpress-logo-32-blue.png)&nbsp;&nbsp;[博客](https://yooooo.us)
