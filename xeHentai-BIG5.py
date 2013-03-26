#!/usr/bin/python2.7
# -*- coding:utf-8 -*-
# A multithread downloader for exhentai.org
# Contributor:
#      fffonion        <fffonion@gmail.com>

__version__=1.31

import urllib,random,threading,httplib2plus as httplib2,\
re,os,Queue,time,os.path as opth,sys,socket,traceback,locale
loginurl='http://e-hentai.org/bounce_login.php?b=d&bt=1-1'
baseurl='http://e-hentai.org'
myhomeurl='http://g.e-hentai.org/home.php'
cooid,coopw,cooproxy,IP,THREAD_COUNT='','','','',5
LOGIN,OVERQUOTA,IS_REDIRECT=False,False,False
LAST_DOWNLOAD_SIZE=[0]*5
def _print(str):
    print(str.decode('utf-8'))
    if argdict['log']:
        f=open(argdict['log'],'a')
        f.write(time.strftime('%m-%d %X : ',time.localtime(time.time()))+str.decode('utf-8').encode('cp936')+'\n')
        f.close()
    
def prompt(str,fill='-'):
    leng=(54-len(str.decode('utf-8')))/2#unicode不等長
    _print((' '*10+fill*leng+str+(leng%2==1 and ' ' or '')+fill*leng).encode('utf-8'))

def _raw_input(str,is_silent=False,default=''):
    if is_silent:return default
    else:
        print(str.decode('utf-8')),
        return raw_input()

def concode(str,errors='ignore'):
    """
    字符串合法化函數
    """
    codec=locale.getdefaultlocale()[1]
    return str.encode(codec,errors)
    #if sys.platform=='win32':return str.encode(codec,errors)
    #else:return str.encode(codec,errors)
    
       
def genheader(custom='',referer=''):
    rrange=lambda a,b,c=1: str(c==1 and random.randrange(a,b) or float(random.randrange(a*c,b*c))/c)
    ua='Mozilla/'+rrange(4,7,10)+'.0 (Windows NT '+rrange(5,7)+'.'+rrange(0,3)+') AppleWebKit/'+rrange(535,538,10)+\
    ' (KHTML, like Gecko) Chrome/'+rrange(21,27,10)+'.'+rrange(0,9999,10)+' Safari/'+rrange(535,538,10)
    ip='%s.%s.%s.%s' % (rrange(0,255),rrange(0,255),rrange(0,255),rrange(0,255))
    headers = {'User-Agent':ua,'Accept-Language':'zh-CN,zh;q=0.8','Accept-Charset':'utf-8;q=0.7,*;q=0.7',\
               'Accept': 'text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8'\
               ,'Connection': 'keep-alive'}#,'X-Forward-For':ip,'Client_IP':ip}
    if cooid and coopw:
        headers['Cookie']='ipb_member_id='+cooid+';'+'ipb_pass_hash='+coopw+';'
        if IS_REDIRECT:
            headers['Referer']=referer or _redirect
            headers['Cookie']+='c[e-hentai.org][/][ipb_member_id]='+cooid+\
            ';'+'c[e-hentai.org][/][ipb_pass_hash]='+coopw+';c[exhentai.org][/][ipb_member_id]='+cooid+\
            ';'+'c[exhentai.org][/][ipb_pass_hash]='+coopw+';s='+cooproxy+';'
    #if coofetcher:headers['Cookie']+=coofetcher
    if 'form' in custom:
        headers['Content-Type']='application/x-www-form-urlencoded'
    return headers

def mkcookie(uname='',key=''):
    if uname and key:silent=True
    else:silent=False
    try:
        logindata={
            'ipb_login_username':_raw_input('輸入用戶名: ',silent,uname),
            'ipb_login_submit':'Login!',
            'ipb_login_password':_raw_input('輸入密碼:   ',silent,key)}
        resp, content = httplib2.Http().request(loginurl, method='POST', headers=genheader('form'),body=urllib.urlencode(logindata))
        coo=resp['set-cookie']
        global cooid,coopw
        cooid=re.findall('ipb_member_id=(.*?);',coo)[0]
        coopw=re.findall('ipb_pass_hash=(.*?);',coo)[0]
        filename=opth.join(getPATH0(),'.ehentai.cookie')
        fileHandle=open(filename,'w')
        fileHandle.write('%s,%s'%(cooid,coopw))
        fileHandle.close()
        prompt('登陸成功!')
        global LOGIN
        LOGIN=True
    except KeyError:
        prompt('登錄失敗!')
    
def getcookie():
    global cooid,coopw
    filename=opth.join(getPATH0(),'.ehentai.cookie')
    if opth.exists(filename):
        fileHandle=open(filename,'r')
        cooall=fileHandle.read()
        cooid,coopw=cooall.split(',')
        fileHandle.close()
        prompt('從文件加載了Cookies')
        global LOGIN
        LOGIN=True
        if opth.getctime(filename)-time.time() >86400:
            http2.request(baseurl, method='GET', headers=genheader())#login to
            prompt('已自動簽到')
            os.remove(filename)
            fileHandle=open(filename,'wb')
            fileHandle.write(cooall)
            fileHandle.close()
        return True
    else:return False

def getpicpageurl(content,pageurl):
    #picpage=re.findall('0 no-repeat"><a href="(.*?)"><img alt=\d+',content)
    picpage=re.findall('<a\shref="([^<>"]*)"><img[^<>]*><br[^<>]*>[0-9]+</a>',content)
    picpagenew=[]
    for i in range(len(picpage)):picpagenew.append(REDIRECT(picpage[i]))
    return picpagenew

def getpicurl(content,pageurl):
    #print content
    picurl=re.findall('<img id="img" src="(.+)".+style="[a-z]',content)[0]
    filename=re.findall('</a></div><div>(.*?) ::.+::.+</di',content)[0]
    if 'image.php' in filename:filename=re.findall('n=(.+)',picurl)[0]
    #http://exhentai.org/fullimg.php?gid=577354&page=2&key=af594b7cf3
    index=re.findall('.+/(\d+)-(\d*)',pageurl)[0]
    fullurl=re.findall('class="mr".+<a href="(.+)"\s*>Download original',content)
    #print pageurl,fullurl
    fullsize=re.findall('Download\soriginal\s[0-9]+\sx\s[0-9]+\s(.*)\ssource',content)#like 2.20MB
    if IS_REDIRECT:
        #print picurl
        #print fullurl
        if forceproxy:picurl=REDIRECT(FIX_REDIRECT(htmldecode(picurl)))
        else:picurl=urllib.unquote(FIX_REDIRECT(htmldecode(picurl)))
        fullurl=fullurl and FIX_REDIRECT(htmldecode(fullurl[0])) or ''
        #print picurl
        #print fullurl
    else:fullurl=fullurl and htmldecode(fullurl[0]) or ''
    elem={'pic':picurl,'full':fullurl\
             ,'name':filename,'gid':index[0],'index':index[1],'fullsize':(fullsize and fullsize[0] or ''),
             'referer':pageurl}
    file=open(opth.join(getPATH0(),legalpath(gname)+'.progress.txt'),'a')
    for i in elem:
        file.write(i+'::'+elem[i]+',')
    file.write('\n')
    file.close()
    return [elem]

def query_info():
    global LOGIN,IP
    if not LOGIN:return
    deltatime=lambda x:time.strftime('%m-%d %X',time.localtime(time.time()+x*60))
    prompt('查詢流量%s信息' % (IP and '' or '及IP'))
    header=genheader()
    try:
        resp,content=httplib2.Http().request(REDIRECT(myhomeurl), method='GET', headers=header)
        if int(resp['status'])>=500:raise Exception('Server Error.')
        used,quota=re.findall('<p>You are currently at <strong>(\d+)</strong> towards a limit of <strong>(\d+)</st',content)[0]
        used,quota=int(used),int(quota)
        _print('當前已使用%d中的%d，%s (可能有延遲)'%(quota,used,\
          (quota<used and ('流量超限，將在%d分鐘後(%s時)恢復，%s時清零。' % \
            (used-quota,deltatime(used-quota),deltatime(used))) or '流量充足')))
        if used>quota and not argdict['force_down']=='y':
            prompt('出現狀況！')
            _raw_input('流量不足, 中斷下載 (可使用-f強制下載), 按回車繼續',is_silent,'')
    except Exception as e:
        _print('暫時無法獲得數據……')
        print e
    if not IP:
        while 1:
            resp,content=httplib2.Http().request(REDIRECT('http://www.whereismyip.com/'),headers=genheader())
            if int(resp['status'])<400:break
            _print('重試……')
        IP=re.findall('\d+\.\d+\.\d+\.\d+',content)[0]
        _print('當前IP %s' % IP)
        
def htmldecode(str):
    return str.replace('&amp;', '&')

def getPATH0():
    """
    返回腳本所在路徑
    """
    if opth.split(sys.argv[0])[1].find('py')!=-1:#is script
        return sys.path[0]
    else:return sys.path[1]
    
def legalpath(str):
    return str.replace('|','').replace(':','').replace('?','').replace('\\','').replace('/','').replace('*','')

def init_proxy(url):
    global cooproxy
    resp,content=httplib2.Http().request(url,headers=genheader())
    cooproxy=re.findall('s=(.*?);',resp['set-cookie'])[0]
    #print cooproxy

def parse_arg(arg_ori):
    arg={'url':'','thread':'5','down_ori':'n','redirect':'','redirect_pattern':'','redirect_norm':'n',\
         'startpos':'1','timeout':'60','force_down':'n','log':'','uname':'','key':''}
    if len(arg_ori)==0:return arg
    if arg_ori[0] in ['--help','-h','/?','-?']:
        _print(\
'''【在線代理】
ehentai對每個ip單位時間內的下載量有配額(一般為120~200)，因此需要使用在線代理來偽裝ip
本下載器支持glype和knproxy兩種類型的在線代理；
glype是目前使用最廣的在線代理，使用時請取消勾選“加密url”、勾選“允許cookies”後隨意打開一個網頁，然後把網址粘貼進來；knproxy是國人開發的一款在線代理，可以使用knproxy的加密模式，用法與glype相同。
【命令行模式】支持命令行模式以方便使用路由器或VPS下載（需要安裝httplib2庫）
參數： ehentai.py url [-t|-o|-r|-p|-rp|-u|-k|-s|-tm|-f|-l]

    url                   下載頁的網址
    -t  --thread          下載線程數，默認為5
    -o  --down-ori        是否下載原始圖片（如果存在）
    -r  --redirect        在線代理的網址，形如http://a.co/b.php?u=xx&b=3
    -ro --redirect-norm   是否應用在線代理到已解析到的非原圖，默認不啟用
    -u  --username        用戶名，覆蓋已保存的cookie
    -k  --key             密碼
    -s  --start-pos       從第幾頁開始下載，默認從頭
    -f  --force           即使超出配額也下載，默認為否
    -l  --logpath         保存日誌的路徑，默認為eh.log
     ----------------------------------------------------------------   
沒什麽大不了的，就是一個批量下圖的東西罷了~
fffonion    <xijinping@yooooo.us>    Blog:http://yooooo.us/
                                                  2013-3-23''')
        os._exit(0)
    try:
        for i in range(len(arg_ori)-1):
            val=arg_ori[i+1].lstrip('"').lstrip("'").rstrip('"').rstrip("'")
            if arg_ori[i]=='-t' or  arg_ori[i]=='--thread':arg['thread']=arg_ori[i+1]
            if arg_ori[i]=='-o' or  arg_ori[i]=='--down-ori':arg['down_ori']='y'
            if arg_ori[i]=='-r' or  arg_ori[i]=='--redirect':arg['redirect']=arg_ori[i+1]
            #if arg_ori[i]=='-p' or  arg_ori[i]=='--redirect-pattern':arg['redirect_pattern']=arg_ori[i+1]
            if arg_ori[i]=='-ro' or  arg_ori[i]=='--redirect-norm':arg['redirect_norm']='y'
            if arg_ori[i]=='-u' or  arg_ori[i]=='--username':arg['uname']=arg_ori[i+1]
            if arg_ori[i]=='-k' or  arg_ori[i]=='--key':arg['key']=arg_ori[i+1]
            if arg_ori[i]=='-s' or  arg_ori[i]=='--start-pos':arg['startpos']=arg_ori[i+1]
            if arg_ori[i]=='-tm' or  arg_ori[i]=='--timeout':arg['timeout']=arg_ori[i+1]
            if arg_ori[i]=='-f' or  arg_ori[i]=='--force':arg['force_down']='y'
            if arg_ori[i]=='-l' or  arg_ori[i]=='--logpath':arg['log']=arg_ori[i+1]
        if arg_ori[0].startswith('http'):arg['url']=arg_ori[0]
        else:
            raise Exception('Illegal URL.')
    except Exception,e:
        _print('錯誤的參數!')
        print e
        arg['url']=''
    #print arg,arg_ori[0]
    return arg
class report(threading.Thread):
    def __init__(self, threadname,reportqueue,monitor_thread):
        threading.Thread.__init__(self, name=threadname)
        self.q=reportqueue
        self.monitor=monitor_thread
    def run(self):
        keep_alive=0
        last_thread=0
        while 1:
            if not self.q.empty():
                reportelem=self.q.get()
                _print('%s - %-8s: %s' % (time.strftime('%X',time.localtime()),reportelem[0], reportelem[1]))
            flag=False
            picthread=0
            for i in self.monitor:
                if i.isAlive():
                    flag=True
                    if '收割機' in i.getName():picthread+=1
            if not flag and self.q.empty():break
            if picthread>0:#不是0早退出了，用於判斷是否pic下載
                if keep_alive==50:
                    keep_alive=0
                    _print('%s - 監視官 :%2d個收割機存活, 共%2d個.' %(time.strftime('%X',time.localtime()),picthread,THREAD_COUNT))
                    #if last_thread==picthread
                    for i in range(len(LAST_DOWNLOAD_SIZE)):
                        samecount=0
                        for j in range(len(LAST_DOWNLOAD_SIZE)):#samecount恰好為相同元素個數
                            if LAST_DOWNLOAD_SIZE[i]==LAST_DOWNLOAD_SIZE[j] and LAST_DOWNLOAD_SIZE[i]!=0:
                                samecount+=1
                        if samecount>=THREAD_COUNT*0.4:
                            prompt('出現狀況！')
                            _raw_input('可能流量已經超限，緊急停止，按回車退出',is_silent,'')
                            os._exit(1)
                keep_alive+=1
            time.sleep(0.2)
            
class download(threading.Thread):
    def __init__(self, threadname,url_queue,save_queue,report_queue,handle_func,father=None):
        threading.Thread.__init__(self, name=threadname)
        self.in_q=url_queue
        self.handle_func=handle_func
        self.out_q=save_queue
        self.prt_q=report_queue
        self.http2=httplib2.Http(opth.join(os.environ.get('tmp'),'.ehentai'))
        self.father=father
        self.picmode='收割機' in self.getName()
    def run(self):
        self.prt_q.put([self.getName(),'已啟動.'])
        sleepseq=[2,4,7,12,15]
        while 1:
            if self.in_q.empty():
                if self.father:
                    if self.father.isAlive():time.sleep(0.5)
                    else:break
                else:break
            slptime=0
            urlori=self.in_q.get()
            if self.picmode:
                name=urlori['name']
                refer=urlori['referer']
                url=urlori[getdowntype()] or urlori['pic']#無original自動切換成pic
            else:
                url=urlori
                name=''
                refer=''
            retries=0
            while 1:
                header=genheader(referer=refer)
                #self.prt_q.put([self.getName(),url])
                try:
                    resp, content = self.http2.request(url, method='GET', headers=header)
                except:
                    if retries<5:
                        self.prt_q.put([self.getName(),'重試'+str(retries+1)+'次……'])
                        retries+=1
                    else:
                        self.prt_q.put([self.getName(),'失敗：'+url])
                        resp,content={'status':'600'},''
                else:
                    if self.picmode:
                            LAST_DOWNLOAD_SIZE[int(self.getName().lstrip('收割機'))-1]=int(resp['content-length'])
                    if self.picmode and len(content)==11:#沒有大圖
                        self.prt_q.put([self.getName(),'木有大圖，下載正常尺寸.'])
                        url=urlori['pic']
                    elif (len(content)<=678 and not self.picmode) or len(content)==925:
                        time.sleep(sleepseq[slptime])
                        slptime=slptime+(slptime==4 and 0 or 1)
                        self.prt_q.put([self.getName(),'等待 %d次. %s'%(slptime,name)])
                    elif len(content)==144 or len(content)==210 or len(content)==1009:
                        self.prt_q.put([self.getName(),'流量超限，請等待一段時間'])
                        self.in_q.put(urlori)
                        return 
                    else:break#正常情況
                time.sleep(sleepseq[retries])
            if int(resp['status'])<400:
                if self.out_q:
                    res=self.handle_func(content,url)
                    for i in res:
                        self.out_q.put(i)
                else:
                    save2file(content,name)
            else:raise Exception('Server Error')
            if self.picmode:self.prt_q.put([self.getName(),'%s (%d) 下載完成.'%(name,len(content))])
            else:self.prt_q.put([self.getName(),url])
        self.prt_q.put([self.getName(),'已退出.'])
        
def save2file(content,name):
    global folder
    filename=opth.join(folder,legalpath(name))
    fileHandle=open(filename,'wb')
    fileHandle.write(content)
    fileHandle.close()
    
  
if __name__=='__main__':
    #try:
    reload(sys)
    sys.setdefaultencoding('utf-8')
    argdict=parse_arg(sys.argv[1:])
    is_silent=(argdict['url'])
    if is_silent:argdict['log']=argdict['log'] or opth.join(getPATH0(),'eh.log')
    prompt('紳♂士下載器 v'+str(__version__),fill=' ')
    if argdict['uname'] and argdict['key']:mkcookie(argdict['uname'],argdict['key'])
    else:
        if not getcookie():
            if _raw_input('當前沒有登陸，要登陸嗎 y/n? (雙倍流量限制,可訪問exhentai)：')=='y':mkcookie()
    while True:
        exurl_all=_raw_input('輸入地址(使用,分割下載多個)：',is_silent,argdict['url']).replace('，'.decode('utf-8'),',') or 'http://g.e-hentai.org/g/577409/208d9b29f7/'
        if exurl_all:break
        prompt('必須輸入地址~')
    if 'exhentai' in exurl_all and not LOGIN:
        if is_silent:
            if argdict['uname'] and argdict['key']:mkcookie(argdict['uname'],argdict['key'])
            else:_print('沒有登錄，無法訪問exhentai')
        else:
            if (_raw_input('需要登錄才能訪問exhentai, 要登陸嗎 y/n?') or 'y')=='y':mkcookie()
    if ',' not in exurl_all:exurl_all=[exurl_all]
    else:exurl_all=exurl_all.split(',')
    THREAD_COUNT=int(_raw_input('設置線程數量(默認5個):',is_silent,argdict['thread']) or '5')
    LAST_DOWNLOAD_SIZE=[0]*THREAD_COUNT
    getdowntype=(_raw_input('是否嘗試下載原圖? y/n(默認):',is_silent,argdict['down_ori']) or 'n')=='y'\
                     and (lambda:'full') or (lambda:'pic')
    startpos=int(_raw_input('從第幾頁開始下載? (默認從頭):',is_silent,argdict['startpos']) or '1')-1
    while True:
        #try:
        fwdsite =_raw_input('輸入中轉站url, 形如http://a.co/b.php?b=4&u=xxx;\
        \n需要允許cookies, 如果出錯請取消加密url; 按回車跳過:',is_silent,argdict['redirect']).rstrip('/')
        if not fwdsite.startswith('http://'):fwdsite='http://'+fwdsite
        if fwdsite!='http://':
            forceproxy=(_raw_input('是否對非原圖也應用中轉? y/n(默認):',is_silent,argdict['redirect_norm']) or 'n')=='y'
            IS_REDIRECT=True
            _redirect,browse=re.findall('(.+)/(.+)\?',fwdsite)[0]
            init_proxy(_redirect+'/'+browse)
            arg=re.findall('([a-zA-Z\._]+)=[^\d]*',fwdsite)[0]
            bval=re.findall('b=(\d*)',fwdsite)
            if bval:bval=bval[0]
            else:bval='4'
            suff=re.findall('http://.+/(.+)',_redirect) or ['']
            suff=suff[0]
            def FIX_REDIRECT(str):
                #picurl不需重定向，fullpicurl需要重定向
                url=re.findall(arg+'=(.*?)&',str+'&')[0]
                if url.find('http')!=-1:#不加密
                    if url.find('fullimg')!=-1:url=REDIRECT(url)
                    return url
                else:#加密,返回/g/browse.php?u=xx，尚未實現
                    if not str.startswith(_redirect):
                       str=(suff and _redirect.replace('/'+suff,'') or _redirect)+str
                    #else:pass
                    return str
        else:
            browse=''
            _redirect='http://'
        def REDIRECT(str):
            if str.startswith(_redirect):return str
            else:
                return _redirect=='http://' \
                    and str \
                    or '%s/%s?b=%s&f=norefer&%s=%s'% (_redirect,browse,bval,arg,str)#.replace('//','/').replace(':/','://')
        query_info()
        break
        #except IndexError:
        #    _print('代理可能有問題，請更換一個~')
        #    continue
        #else:break
    for exurl in exurl_all:
        http2=httplib2.Http(opth.join(os.environ.get('tmp'),'.ehentai'))
        resp, content = http2.request(exurl, method='GET', headers=genheader())
        #h1 id="gn">[DISTANCE] HHH Triple H Archetype Story [german/deutsch]</h1>
        gname=re.findall('="gn">(.*?)</h1>',content)[0].decode('utf-8')
        _print('Sibylla system: 準備下載 '+gname)
        folder=opth.join(getPATH0(),legalpath(gname)).decode('utf-8')
        if not opth.exists(folder):os.mkdir(folder)
        pagecount=re.findall('<a href="'+exurl+'\?p=\d*" onclick="return false">(.*?)</a></td'\
                ,content)
        if len(pagecount)<=1:pagecount= 1
        else:pagecount=int(pagecount[-2])
        #print gname,pagecount#first none;page 2 ?p=1
        reportqueue=Queue.Queue()
        picpagequeue=Queue.Queue()
        picqueue=Queue.Queue()
        urlqueue=Queue.Queue()
        if opth.exists(opth.join(getPATH0(),legalpath(gname)+'.progress.txt')):
            os.remove(opth.join(getPATH0(),legalpath(gname)+'.progress.txt'))
        if opth.exists(opth.join(getPATH0(),legalpath(gname)+'.txt')) and getdowntype()=='full':#非完整圖已變成509
            downthread=[download('收割機%d'% (i+1),picqueue,None,reportqueue,None) for i in range(THREAD_COUNT)]
            rpt=report('監視官',reportqueue,downthread)
            file=open(opth.join(getPATH0(),legalpath(gname)+'.txt'),'r')
            for line in file:
                elem={}
                sec=line.split(',')
                for i in sec:
                    if ':' in i:
                        j=i.split('::')
                        if j[0]=='full' and \
                        (j[1].startswith('http://g.e-hentai.org/fullimg.php') \
                         or j[1].startswith('http://exhentai.org/fullimg.php')) and IS_REDIRECT:#重建規則
                            elem[j[0]]=REDIRECT(j[1])
                        else:elem[j[0]]=j[1]
                picqueue.put(elem)  
                pagenum=picqueue.qsize()
            file.close()
        else:
            for i in range(pagecount-startpos):urlqueue.put(exurl+'?p='+str(i+startpos))#第一頁可以用?p=0
            pagethread=download('執行官',urlqueue,picpagequeue,reportqueue,getpicpageurl)
            rpt=report('監視官',reportqueue,[pagethread])
            pagethread.start()
            rpt.start()
            pagethread.join()
            rpt.join()
            deeperthread=download('執行官+',picpagequeue,picqueue,reportqueue,getpicurl)
            deeperthread.start()#deeperthread沒有join了
            downthread=[download('收割機%d'% (i+1),picqueue,None,reportqueue,None,father=deeperthread) for i in range(THREAD_COUNT)]
            rpt=report('監視官',reportqueue,[deeperthread]+downthread)
            pagenum=picpagequeue.qsize()+1
        #while not picqueue.empty():print picqueue.get()
        prompt('下載開始. 大約下載 %d 張圖片' %(pagenum))
        if not OVERQUOTA:
            for i in range(THREAD_COUNT):downthread[i].start()
        rpt.start()
        if not OVERQUOTA:
            for i in range(THREAD_COUNT):downthread[i].join()
        rpt.join()
        prompt('下載結束.')
        if opth.exists((opth.join(getPATH0(),legalpath(gname)+'.txt'))):
            os.remove((opth.join(getPATH0(),legalpath(gname)+'.txt')))
        if opth.exists((opth.join(getPATH0(),legalpath(gname)+'.progress.txt'))):
            os.rename(opth.join(getPATH0(),legalpath(gname)+'.progress.txt'),\
                       opth.join(getPATH0(),legalpath(gname)+'.txt'))
        _print(gname+' 下載完成！')
        query_info()
    '''except:
        if argdict['log']:
            f=open(argdict['log'],'a')
            f.write(time.strftime('%m-%d %X : ',time.localtime(time.time()))+\
                    '發生錯誤:\n '.decode('utf-8').encode('cp936'))
            traceback.print_exc(file=f)
            traceback.print_exc()
            f.flush()
            f.close()'''
_raw_input('\n按回車鍵退出……',is_silent,'')
os._exit(0)