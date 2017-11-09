# 绅♂士♂站♂小♂爬♂虫

[![Build Status](https://travis-ci.org/fffonion/xeHentai.svg?branch=master)](https://travis-ci.org/fffonion/xeHentai)

[English](README.md) [繁體中文](README.cht.md)

[xeHentai Web界面](https://github.com/fffonion/xeHentai-webui)

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

常用参数: 

 - **daemon** 后台模式，仅支持posix兼容的系统，参见[运行模式](#运行模式)，默认为否
 - **dir** 下载目录，默认为当前目录
 - **download_ori** 是否下载原图，默认为否
 - **jpn_title** 是否使用日语标题，如果关闭则使用英文或罗马字标题，默认为是
 - **rename_ori** 将图片重命名为原始名称，如果关闭则使用序号，默认为否

高级参数: 

 - **proxy** 代理列表，参见[代理](#代理)。
 - **proxy_image** 是否同时使用代理来下载图片和扫描网页，默认为是
 - **proxy_image_only** 是否仅使用代理来下载图片，不用于扫描网页，默认为否
 - **rpc_interface** RPC绑定的IP，参见[JSON-RPC](#json-rpc)，默认为`localhost`
 - **rpc_port** RPC绑定的端口，默认为`None`
 - **rpc_secret** RPC密钥，默认为`None` (不开启RPC服务器)
 - **delete_task_files** 是否删除任务时同时删除下载的文件，默认为否
 - **make_archive** 是否下载完成后生成zip压缩包，并删除下载目录，默认为否
 - **download_range** 设置下载的图片范围，参见[下载范围](#下载范围)
 - **scan_thread_cnt** 扫描线程数，默认为`1`
 - **download_thread_cnt** 下载线程数，默认为`5`
 - **download_timeout** 设置下载图片的超时，默认为`10`秒
 - **ignored_errors** 设置忽略的错误码，默认为空，错误码可以从`const.py`中获得
 - **log_path** 日志路径，默认为`eh.log`
 - **log_verbose** 日志等级，可选1-3，值越大输出越详细，默认为`2`
 - **save_tasks** 是否保存任务到`h.json`，可用于断点续传，默认为否
 

### 命令行模式
```
用法: xeH [-u USERNAME] [-k KEY] [-c COOKIE] [-i] [--daemon] [-d DIR] [-o]
           [-j BOOL] [-r BOOL] [-p PROXY] [--proxy-image | --proxy-image-only]
           [--rpc-interface ADDR] [--rpc-port PORT] [--rpc-secret ...]
           [--delete-task-files BOOL] [-a BOOL] [--download-range a-b,c-d,e]
           [-t N] [--timeout N] [-f] [-l /path/to/eh.log] [-v] [-h]
           [--version]
           [url [url ...]]

绅♂士下载器

必选参数:
  url                   下载页的网址

可选参数:
  -u USERNAME, --username USERNAME
                        用户名
  -k KEY, --key KEY     密码
  -c COOKIE, --cookie COOKIE
                        Cookie字符串, 如果指定了用户名和密码, 此项会被忽略
  -i, --interactive     交互模式, 如果开启后台模式, 此项会被忽略 (默认: False)
  --daemon              后台模式 (默认: False)
  -d DIR, --dir DIR     设置下载目录 (默认: 当前目录)
  -o, --download-ori    是否下载原始图片（如果存在）, 需要登录 (默认: False)
  -j BOOL, --jpn-title BOOL
                        使用日语标题, 如果关闭则使用英文或罗马字标题 (默认: True)
  -r BOOL, --rename-ori BOOL
                        将图片重命名为原始名称, 如果关闭则使用序号 (默认: False)
  -p PROXY, --proxy PROXY
                        设置代理, 可以指定多次, 当前支持的类型: socks5/4a, http(s), glype.
                        代理默认只用于扫描网页 (默认: 空)
  --proxy-image         同时使用代理来下载图片和扫描网页（默认: True)
  --proxy-image-only    仅使用代理来下载图片, 不用于扫描网页 (默认: False)
  --rpc-interface ADDR  设置JSON-RPC监听IP (默认: localhost)
  --rpc-port PORT       设置JSON-RPC监听端口 (默认: None)
  --rpc-secret ...      设置JSON-RPC密钥 (默认: None)
  --delete-task-files BOOL
                        删除任务时同时删除下载的文件 (默认: False)
  -a BOOL, --archive BOOL
                        下载完成后生成zip压缩包并删除下载目录 (默认: False)
  --download-range a-b,c-d,e
                        设置下载的图片范围, 格式为 开始位置-结束位置, 或者单张图片的位置, 使用逗号来分隔多个范围, 例如
                        5-10,15,20-25, 默认为下载所有
  -t N, --thread N      下载线程数 (默认: 5)
  --timeout N           设置下载图片的超时 (默认: 10秒)
  -f, --force           忽略配额判断, 继续下载 (默认: False)
  -l /path/to/eh.log, --logpath /path/to/eh.log
                        保存日志的路径 (默认: eh.log)
  -v, --verbose         设置日志装逼等级 (默认: 2)
  -h, --help            显示本帮助信息
  --version             显示版本信息

```

如果参数未指定, 则使用config.py中的默认值；否则将覆盖config.py设置的值。

### JSON-RPC

在指定`rpc_interface`和`rpc_port`后, xeHentai会启动RPC服务器。使用[JSON-RPC 2.0](http://www.jsonrpc.org/specification)标准。典型的请求如下：

```
$ curl localhost:8010/jsonrpc -d '{"jsonrpc": "2.0", "id": 1, "method":"xeH.addTask", "params":[[args],{kwargs}]}'
{"jsonrpc": "2.0", "id": 1, "result": "36df423e"}
```

`rpc_secret`可用于提高安全性。如果`rpc_secret`设置为**hentai**, 则需在params中带上这个值：
```
$ curl localhost:8010/jsonrpc -d '{"jsonrpc": "2.0", "id": 1, "method":"xeH.addTask", "params":["token:hentai",[args],{kwargs}]}'
{"jsonrpc": "2.0", "id": 1, "result": "36df423e"}
```

其中`method`为调用的方法，必须以**xeH.** 开头。在[core.py](xeHentai/core.py)的xeHentai类中，所有不以下划线`_`开头的方法均可以通过RPC调用，但需将方法名的下划线命名法改为驼峰命名法。如`add_task`需改为`addTask`。

参数列表请参阅xeHentai类。

如果浏览器安装了用户脚本插件，可以[下载xeHentaiHelper.user.js](http://dl.yooooo.us/userscripts/xeHentaiHelper.user.js)，将会在页面上添加`Add to xeHentai`链接，以支持将当前页面添加到xeHentai中。Chrome用户需要安装[Tampermonkey](https://chrome.google.com/webstore/detail/tampermonkey/dhdgffkkebhmkfjojejmpbldmpobfkfo)，
Firefox用户需要安装[Greasemonkey](https://addons.mozilla.org/en-US/firefox/addon/greasemonkey/)，Opera和傲游用户需要安装暴力猴。

**由于绅士站启用了https，而rpc走的是http，所以chrome用户需要点击地址栏右侧盾牌，选择“加载不安全的脚本”**

### 运行模式

如果通过命令行或交互模式指定了下载url，xeHentai会在下载完成`h.json`中存储的任务（如果存在）及指定的url后退出。

如果命令行没有指定url，xeHentai将会在完成存档`h.json`中的队列（如果存在）后继续等待。

如果指定了后台模式（`-d`或设置`daemon`为`True`），xeHentai将会在保持后台运行。

### 代理

目前支持三种模式的代理: 

 - socks代理，如`socks5h://127.0.0.1:1080`；如果需要在客户端解析DNS，请使用`socks5://127.0.0.1:1080`。
 - http(s)代理，如`http://127.0.0.1:8080`。
 - glype代理，如`http://example.com/browse.php?u=a&b=4`。请根据实际情况修改`b`的名称。glype是目前使用最广的php在线代理，使用时请取消勾选“加密url(Encrypt URL)”、取消勾选“移除脚本 (Remove Scripts)”、勾选“允许cookies (Allow Cookies)”后随意打开一个网页，然后把网址粘贴进来

可以指定多个代理，格式如`['socks5h://127.0.0.1:1080', 'http://127.0.0.1:8080']`。

默认情况下代理会被用于扫描网页和下载图片。如果不需要使用代理下载图片，请在配置文件中设置`proxy_image`为**False**。

如果使用代理仅用于突破封锁的目的，则此项可以设置为`False`；如果需要保证隐私，请将此项设置为`True`。使用glype代理的用户建议将此项设为`False`。

如果仅需要使用代理下载图片，不需要扫描网页，请在配置文件中设置`proxy_image_only`为**True**，或者在运行时加上`--proxy-image-only`参数。如果在配置中的`proxy_image`和`proxy_image_only`均为**True**，则`proxy_image`将被忽略。

### 下载范围

下载范围的格式为使用`开始位置-结束位置`，例如`5-10`表示下载第5到第10张图片，包括第5和第10张；或者单个位置，例如`15`表示下载第15张图片。

可以通过逗号来分割多个范围，例如`5-10,15`表示下载第5到第10张图片以及第15张图片。

如果不输入下载范围，则默认下载所有图片。


## 其他说明

### 配额

直接从服务器及镜像途径下载的图片计入配额，从H@H下载的不计算；下载新发布的、冷门的漫画以及原图更有可能消耗配额，下载热门漫画基本不消耗配额

## License

GPLv3
***
![@fffonion](http://img.t.sinajs.cn/t5/style/images/register/logo.png)[@fffonion](http://weibo.com/376463435)&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;![Blog](https://s.w.org/about/images/logos/wordpress-logo-32-blue.png)&nbsp;&nbsp;[博客](https://yooooo.us)
