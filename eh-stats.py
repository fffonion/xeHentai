#coding:utf-8
import urllib2,os,os.path as opth,time,sys,random,re

def getPATH0():
    return sys.path[0]
global cooid,coopw
filename=opth.join(getPATH0(),'.ehentai.cookie')

fileHandle=open(filename,'r')
cooall=fileHandle.read()
cooid,coopw=cooall.split(',')
fileHandle.close()

rrange=lambda a,b,c=1: str(c==1 and random.randrange(a,b) or float(random.randrange(a*c,b*c))/c)
ua='Mozilla/'+rrange(4,7,10)+'.0 (Windows NT '+rrange(5,7)+'.'+rrange(0,3)+') AppleWebKit/'+rrange(535,538,10)+\
' (KHTML, like Gecko) Chrome/'+rrange(21,27,10)+'.'+rrange(0,9999,10)+' Safari/'+rrange(535,538,10)
ip='%s.%s.%s.%s' % (rrange(0,255),rrange(0,255),rrange(0,255),rrange(0,255))
headers = {'User-Agent':ua,'Accept-Language':'zh-CN,zh;q=0.8','Accept-Charset':'utf-8;q=0.7,*;q=0.7',\
		   'Accept': 'text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8'\
		   ,'Connection': 'keep-alive'}#,'X-Forward-For':ip,'Client_IP':ip}
headers['Cookie']='ipb_member_id='+cooid+';'+'ipb_pass_hash='+coopw+';'
while True:
	file=open(opth.join(getPATH0(),'stat.csv'),'a')
	req = urllib2.Request('http://g.e-hentai.org/home.php')
	for i in headers:req.add_header(i,headers[i])
	content=urllib2.urlopen(req).read()
	used,quota=re.findall('<p>You are currently at <strong>(\d+)</strong> towards a limit of <strong>(\d+)</st',content)[0]
	str=('%s,%s') % (time.strftime('%m-%d,%A,%H:%M',time.localtime(time.time())) ,quota)
	print '[E-Hentai stats] '+str
	file.write(str+'\n')
	file.flush()
	file.close()
	time.sleep(180)