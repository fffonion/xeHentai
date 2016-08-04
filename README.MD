# 绅♂士♂站♂小♂爬♂虫

## 下载

windows用户可以下载可执行文件 [这里](https://github.com/fffonion/xeHentai/releases) [或这里](http://dl.yooooo.us/share/xeHentai/)

或者可以运行源码

```shell
pip install -U requests[socks]
git clone https://github.com/fffonion/xeHentai
cd xeHentai
python ./setup.py install
```

## 配额

直接从服务器及镜像途径下载的图片计入配额，从H@H下载的不计算；下载新发布的、冷门的漫画更有可能消耗配额，下载热门漫画基本不消耗配额

![quota](http://ww3.sinaimg.cn/large/436919cbjw1e314v6gxtzj.jpg)

## 命令行模式
```
用法: xeH [-h] [-u USERNAME] [-k KEY] [-c COOKIE] [-i] [-o] [-t N] [-f]
           [-d DIR] [--daemon] [-l /path/to/eh.log] [-p PROXY] [-v]
           [--rpc-port PORT] [--rpc-interface ADDR]
           [url [url ...]]

绅♂士下载器

可选参数:
  url                   下载页的网址
  -h, --help            show this help message and exit
  -u USERNAME, --username USERNAME
                        用户名
  -k KEY, --key KEY     密码
  -c COOKIE, --cookie COOKIE
                        Cookie字符串，如果指定了用户名和密码，此项会被忽略
  -i, --interactive     交互模式，如果开启后台模式，此项会被忽略 (默认: False)
  -o, --download-ori    是否下载原始图片（如果存在） (默认: False)
  -t N, --thread N      下载线程数 (当前: 5)
  -f, --fast-scan       从hathdl猜测页面链接，可以提高抓取速度，但有时会抽风 (默认: False)
  -d DIR, --dir DIR     设置下载目录 (默认: 当前目录)
  --daemon              后台模式 (默认: False)
  -l /path/to/eh.log, --logpath /path/to/eh.log
                        保存日志的路径 (默认: eh.log)
  -p PROXY, --proxy PROXY
                        设置代理, 当前支持的类型: socks5/4a, http(s) (默认: 无)
  -v, --verbose         设置日志装逼等级 (默认: 2)
  --rpc-port PORT       设置JSON-RPC监听IP (默认: 无)
  --rpc-interface ADDR  设置JSON-RPC监听端口 (默认: localhost)

如果参数未指定，则使用config.py中的默认值
```   

新版本默认为命令行模式，如果需要使用交互模式，请运行`xeH.py -i`

## License

GPLv3
***
![@fffonion](http://img.t.sinajs.cn/t5/style/images/register/logo.png)[@fffonion](http://weibo.com/376463435)&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;![Blog](https://s.w.org/about/images/logos/wordpress-logo-32-blue.png)&nbsp;&nbsp;[博客](https://yooooo.us)
