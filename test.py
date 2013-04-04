# -*- coding:utf-8 -*-
import re
#http://exhentai.org/g/575649/3fcd227ec7/
class HatH_hdl():
    class picelem():
        def __init__(self,str,gid):
            self.id,self.hash,self.length,self.width,self.height,self.format,self.name\
            =re.findall('(\d+) ([a-z0-9]+)-(\d+)-(\d+)-(\d+)-([a-z]+) (.+)',str)[0]
            self.id,self.length,self.width,self.height=\
                int(self.id),int(self.length),int(self.width),int(self.height)
            self.hash_s=self.hash[:10]
            self._gid=gid
        def __str__(self):
            return '%s %s %s %s %s %s %s'%\
                (self.id,self.hash,self.length,self.width,self.height,self.format,self.name)
                
        def url(self,home=''):
            #http://exhentai.org/s/ba1c7d5cda/575649-1
            #if file resized, this url not works
            return 'http://%s.org/s/%s/%d-%d'%\
                ((home=='ex' and 'exhentai' or 'g.e-hentai'),self.hash_s,self._gid,self.id)
                
    def __init__(self,filename):
        self.file=open(filename,'r')
        self.content=self.file.read()
        try:
            self._name=re.findall('TITLE (.+)',self.content)[0]
            self._gid=int(re.findall('GID (.+)',self.content)[0])
            self._count=int(re.findall('FILES (.+)',self.content)[0])
            self._title=re.findall('Title:\s+(.+)',self.content)[0]
            self._upload_time=re.findall('Upload Time:\s+(.+)',self.content)[0]#invisible
            self._upload_by=re.findall('Uploaded By:\s+(.+)',self.content)[0]#invisible
            self._downloaded=re.findall('Downloaded:\s+(.+)',self.content)[0]#invisible
            self._tags=re.findall('Tags:\s+(.+)',self.content)[0].split(', ')
            listtmp=re.findall('FILELIST\n(.+)\n+\nINFORMATION',self.content,re.DOTALL)[0]
            listtmp=listtmp.split('\n')
        except IndexError,ValueError:
            print('File Illegal.')
        self._piclist=[self.picelem(listtmp[i],self.gid) for i in range(len(listtmp))]
        print self._piclist[0].url('ex')
        
    def __len__(self):
        return self._count
    
    def __getattr__(self,key):
        if key=='count':return self._count
        if key=='gid':return self._gid
        if key=='name':return self._name
        if key=='title':return self._title
        if key=='tags':return self._tags
        if key=='list':return self._piclist

        
    