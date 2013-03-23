#!/usr/bin/env python
#-*- coding:utf-8 -*-
import httplib,urllib,urllib2,cookielib
import os,re,sys,time,socket

def ProxyDown(addr,path,proxy='xionghaizi.3owl.com/gg'):
	'''
	使用网页代理抓取网页
	不加密模式：/browse.php?u=[encoded url]&b=4[&f=norefer]
	在线代理站：
	http://la.huoliquankai.info/
	'''
	def GetSize(str):
		'''
		形如"xxx KB"/"xxx MB"的字符串，返回大约的字节数
		'''
		mem=str.split(' ')
		size=float(mem[0])*1024
		if mem[1][0]=='M':size*=1024
		return size
	#规则：获取图片数量
	num=re.compile('Showing\s1\s-\s[0-9]+\sof\s([0-9]+)\simages')
	#规则：获取目录分页面
	idx=re.compile('<a\shref="([^<>"]*)"><img[^<>]*><br[^<>]*>[0-9]+</a>')
	#规则：获取下一页漫画页面
	nxt=re.compile('<a\shref="([^<>]*)"><img\ssrc="[^<>]*"\sstyle="[^<>]*"\s/></a>')
	#规则：从漫画页面获取图片地址
	adr=re.compile('<a\shref="[^<>]*"><img\ssrc="([^<>]*)"\sstyle="[^<>]*"\s/></a>')
	#规则：从漫画页面获取图片大小
	dsz=re.compile('<div>.*\s::\s[0-9]+\sx\s[0-9]+\s::\s(.*)</div>')
	#规则：如果存在原始大图，获取其地址
	pic=re.compile('<a\shref="([^<>"]*)">Download\soriginal')
	#规则：如果存在原始大图，获取其大小
	psz=re.compile('Download\soriginal\s[0-9]+\sx\s[0-9]+\s(.*)\ssource')
	#设置cookie
	cookie_support=urllib2.HTTPCookieProcessor(cookielib.CookieJar())
	#设置opener
	opener=urllib2.build_opener(cookie_support,urllib2.HTTPHandler)
	#设置header
	header={
	'User-Agent':'Mozilla/4.0 (compatible; MSIE 8.0; Windows NT 5.1; Trident/4.0; .NET CLR 2.0.50727)',
	'Accept-Language':'zh-CN,zh;q=0.8',
	'Accept-Charset':'utf-8;q=0.7,*;q=0.7',
	'Host':proxy,
	'connection':'keep-alive'}
	#网页代理中原网站用GET方式提交
	query={
	'u':'http://e-hentai.org/bounce_login.php?b=d&bt=1-1',
	'b':'4',
	'f':'norefer'}
	#EHentai账号与密码，没有的话删掉这个和下面的登录部分，代价是不能从exhentai下
	login={
	'ipb_login_password':'1993x429',#填入你的密码
	'ipb_login_submit':'Login!',
	'ipb_login_username':'fffonion'}#换成你的账户
	#开启代理主页面
	try:
		Url='http://%s/index.php'%(proxy)
		req=urllib2.Request(Url,headers=header)
		opener.open(req).read()
		print unicode('成功打开代理网站！','utf-8')
	except urllib2.HTTPError,e:
		print unicode('无法打开代理网站 : %s'%(e),'utf-8')
		sys.exit(1)
	#登录E-Hentai网站
	try:
		header['Referer']=Url
		Url='http://%s/browse.php?%s'%(proxy,urllib.urlencode(query))
		req=urllib2.Request(Url,data=urllib.urlencode(login),headers=header)
		opener.open(req).read()
		print unicode('成功登录绅士(表)！','utf-8')
		del query['f']
		header['Referer']=Url
		query['u']='http://exhentai.org/'
		Url='http://%s/browse.php?%s'%(proxy,urllib.urlencode(query))
		req=urllib2.Request(Url,headers=header)
		opener.open(req).read()
		print unicode('成功登录绅士(里)！','utf-8')
	except urllib2.HTTPError,e:
		print unicode('登录失败 : %s'%(e),'utf-8')
		sys.exit(1)
	#建立存放目录
	path='\\'.join(path.split('\\'))
	if not os.path.isdir(path):os.mkdir(path)
	path+='\\'
	#进入下载循环
	try:
		#打开目录页面
		header['Referer']=Url
		query['u']=addr
		Url='http://%s/browse.php?%s'%(proxy,urllib.urlencode(query))
		req=urllib2.Request(Url,headers=header)
		content=opener.open(req).read()
		NUM=num.findall(content)
		PGE=idx.findall(content)
		if NUM and PGE:
			NUM=int(NUM[0])
			print unicode('共发现 %03d 张图片！'%(NUM),'utf-8')
			t=0
			for i in xrange(NUM):
				while 1:
					try:
						if t>10:
							print unicode('累计10次错误，退出！','utf-8')
							sys.exit(1)
						header['Referer']=Url
						req=urllib2.Request(PGE[0],headers=header)
						content=opener.open(req).read()
						print unicode('第 %03d 张页面已打开'%(i+1),'utf-8'),
						name='%s%03d.jpg'%(path,i+1)
						if os.path.isfile(name):
							print unicode('图片已经存在','utf-8')
						else:
							PIC=pic.findall(content)
							if PIC:
								PSZ=psz.findall(content)
								print unicode('--> [高清版] -->','utf-8'),
								header['Referer']=Url
								query['u']=PIC[0]
								req=urllib2.Request(PIC[0],headers=header)
								photo=opener.open(req).read()
								if float(len(photo))*1.02<GetSize(PSZ[0]):
									print unicode('Download Error','utf-8')
									time.sleep(5)
									t+=1
									continue
								else:
									print unicode('下载完毕','utf-8')
									file=open(name,'wb')
									file.write(photo)
									file.close()
							else:
								ADR=adr.findall(content)
								if ADR:
									DSZ=dsz.findall(content)
									print unicode('--> [普通版] -->','utf-8'),
									req=urllib2.Request(ADR[0],headers=header)
									photo=opener.open(req).read()
									if float(len(photo))*1.02<GetSize(DSZ[0]):
										print unicode('Download Error','utf-8')
										time.sleep(5)
										t+=1
										continue
									else:
										print unicode('下载完毕','utf-8')
										file=open(name,'wb')
										file.write(photo)
										file.close()
								else:
									print unicode('该页面未找到图片！','utf-8')
						Url=PGE[0]
						PGE=nxt.findall(content)
						if PGE:
							t=0
							break
						else:
							print unicode('未找到下一页图片！','utf-8')
							time.sleep(5)
							t+=1
					except socket.error,e:
						print unicode('Socket Error %s'%(e),'utf-8')
						time.sleep(5)
						t+=1
					except urllib2.URLError,e:
						print unicode('URL Error %s'%(e),'utf-8')
						time.sleep(5)
						t+=1
					except httplib.BadStatusLine,e:
						print unicode('Status Error','utf-8')
						time.sleep(5)
						t+=1
		else:
			print unicode('未发现图片！','utf-8')
	except urllib2.HTTPError,e:
		print unicode('图片下载失败 : %s'%(e),'utf-8')
		sys.exit(1)
	
if __name__=='__main__':
	addr=raw_input('Input URL : ')
	path=raw_input('Input DIR : ')
	addr=unicode(addr,'utf-8')
	path=unicode(path,'utf-8')
	ProxyDown(addr,path)