【在线代理】
ehentai对每个ip单位时间内的下载量有配额(一般为120~200)，因此需要使用在线代理来伪装ip
本下载器支持glype和knproxy两种类型的在线代理；
glype是目前使用最广的在线代理，使用时请取消勾选“加密url”、勾选“允许cookies”后随意打开一个网页，然后把网址粘贴进来；knproxy是国人开发的一款在线代理，可以使用knproxy的加密模式，用法与glype相同。
【命令行模式】支持命令行模式以方便使用路由器或VPS下载（需要安装httplib2库）
参数： ehentai.py url [-t|-o|-r|-p|-rp|-u|-k|-s|-tm|-f|-l]

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
     ----------------------------------------------------------------   
没什么大不了的，就是一个批量下图的东西罢了~
fffonion    <xijinping@yooooo.us>    Blog:http://yooooo.us/