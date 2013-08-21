#绅♂士♂站♂小♂爬♂虫

##注意！！

适！度！使！用！不要设过大的线程数不然IP会被BAN！

##下载

windows可执行文件: [这里这里](https://github.com/fffonion/xeHentai/raw/master/release/xeHentai.exe)
   
python脚本: [这里这里](https://github.com/fffonion/xeHentai/raw/master/xeHentai.py)


其他：(EXE不需要)

依赖库[这里](https://github.com/fffonion/xeHentai/raw/master/dependency.zip)

httplib2库[这里](https://github.com/fffonion/xeHentai/raw/master/httplib2plus.zip)

ehentai配额统计脚本：python脚本[这里](https://github.com/fffonion/xeHentai/raw/master/xeH-stats.py)


##在线代理

ehentai对每个ip单位时间内的下载量有配额(一般为120~200)，因此需要使用在线代理来伪装ip
***
本下载器支持glype和knproxy两种类型的在线代理；
glype是目前使用最广的php在线代理，使用时请取消勾选“加密url”、勾选“允许cookies”后随意打开一个网页，然后把网址粘贴进来；
knproxy是[KnH](http://kanoha.org/knproxy/)开发的一个基于php的在线代理，可以使用knproxy的加密模式，用法与glype相同。

##配额

直接从服务器及镜像途径下载的图片计入配额，从H@H下载的不计算；下载新发布的、冷门的漫画更有可能消耗配额，下载热门漫画基本不消耗配额

![quota](http://ww3.sinaimg.cn/large/436919cbjw1e314v6gxtzj.jpg)

##命令行模式

支持命令行模式以方便使用路由器或VPS下载（需要安装httplib2库）
***
参数： xeHentai.py url [-t|-o|-r|-p|-rp|-u|-k|-s|-tm|-f|-l]

    url                   下载页的网址
    -t  --thread          下载线程数，默认为5
    -o  --down-ori        是否下载原始图片（如果存在）
    -r  --redirect        在线代理的网址，形如http://a.co/b.php?u=xx&b=3
    -ro --redirect-norm   是否应用在线代理到已解析到的非原图，默认不启用
    -u  --username        用户名，覆盖已保存的cookie
    -k  --key             密码
    -s  --start-pos       从第几页开始下载，默认从头
    -f  --force           即使超出配额也下载，默认为否
    -l  --logpath         保存日志的路径，默认为eh.log
	-re --rename          是否重命名成原始文件名
     ----------------------------------------------------------------   
	 
设置线程数4，下载原图，从第三页开始，下载完成后重命名成原始文件名：

	xeHentai.py http://exhentai.org/g/613908/1f1864b790/ -t 4 -o -s 3 -re
	
	xeHentai.py http://exhentai.org/g/613908/1f1864b790/ --thread=4 --down-ori --start-pos=3 --rename

##License

[CC BY-3.0](http://zh.wikipedia.org/wiki/Wikipedia:CC_BY-SA_3.0协议文本)
***
![@fffonion](http://img.t.sinajs.cn/t5/style/images/register/logo.png)[@fffonion](http://weibo.com/376463435)&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;![Blog](http://zmingcx.com/wp-content/themes/HotNewspro/images/caticon/wordpress.gif)[博客](http://www.yooooo.us)
