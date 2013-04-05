#!/usr/bin/python2.7
# -*- coding:utf-8 -*-
# A file parser for h@h(.hathdl)
# Contributor:
#      fffonion        <fffonion@gmail.com>

__version__=1.0
import re,os,os.path as opth
#http://exhentai.org/g/575649/3fcd227ec7/
class HatH(object):
    class picelem(object):
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
                
        def url(self,isEX=False):
            #http://exhentai.org/s/ba1c7d5cda/575649-1
            #if file resized, this url not works
            return 'http://%s.org/s/%s/%d-%d'%\
                ((isEX and 'exhentai' or 'g.e-hentai'),self.hash_s,self._gid,self.id)
                
    def __init__(self,dirpath='',filename='',hathcontent='',check=True):
        '''
        dirpath must be utf-8 decoded
        '''
        if filename:
            self.file=open(filename,'r')
            self.content=self.file.read()
            self.file.close()
        else:self.content=hathcontent
        self.check=check
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
            print('File or content illegal.')
        self.setpath(dirpath)
        self.genlist(listtmp,check)
        
    def setpath(self,path):
        if path:self.__setattr__('path',path)
        else:self.__setattr__('path',self._name)
        
    def genlist(self,raw_list,check=True):
        #生成图片列表
        self._piclist=[self.picelem(raw_list[i],self.gid) for i in range(len(raw_list))]
        self._remainindex=self._checkexist(self.dirpath,check)
        #re-generate
        self._piclist_veryfied=[self._piclist[i] for i in self._remainindex]
    
    def genindex(self):
        return self._checkexist(self.dirpath,self.check)
    
    def _checkexist(self,path,check):
        tobedown_index=[]
        for i in range(len(self._piclist)):
            #if not opth.exists(opth.join(path,self._piclist[i].name)) or not check:
            if not opth.exists(opth.join(path,'%03d.%s'%\
                            (self._piclist[i].id,self._piclist[i].format))) or not check:
                tobedown_index+=[i]
        return tobedown_index
    
    def renameToSeq(self,overwrite=False):
        return self._renameToSeq(self.dirpath,overwrite)
    
    def _renameToSeq(self,path,overwrite=False):
        for i in range(self._count):
            oriname=self._piclist[i].name
            seqname='%03d.%s'%(int(self._piclist[i].id),self._piclist[i].format)
            if opth.exists(opth.join(path,oriname)):
                if opth.exists(opth.join(path,seqname)):
                    i=1
                    while opth.exists(opth.join(path,seqname.replace('.','-%d.'%i))):
                        i+=1
                    os.rename(opth.join(path,oriname), opth.join(path,seqname.replace('.','-%d.'%i)))
                else:  
                    os.rename(opth.join(path,oriname), opth.join(path,seqname))
            
    def renameToOri(self,overwrite=False):
        return self._renameToOri(self.dirpath,overwrite)
    
    def _renameToOri(self,path,overwrite=False):
        for i in range(self._count):
            oriname=self._piclist[i].name
            seqname='%03d.%s'%(int(self._piclist[i].id),self._piclist[i].format)
            if opth.exists(opth.join(path,seqname)):
                if opth.exists(opth.join(path,oriname)):
                    i=1
                    while opth.exists(opth.join(path,oriname.replace('.','-%d.'%i))):
                        i+=1
                    os.rename(opth.join(path,seqname), opth.join(path,oriname.replace('.','-%d.'%i)))
                else:        
                    os.rename(opth.join(path,seqname), opth.join(path,oriname))
                   
    def __len__(self):
        return self._count
    
    def __getattr__(self,key):
        if key=='count':return self._count
        elif key=='gid':return self._gid
        elif key=='name':return self._name
        elif key=='title':return self._title
        elif key=='tags':return self._tags
        elif key=='list':return self._piclist_veryfied
        else:return object.__getattr__(self,key)
        
    def __setattr__(self, name, value):
        if name=='path':self.dirpath=value
        if name=='name':self._name=value
        else:object.__setattr__(self,name,value)
    
if __name__=='__main__':
    def getPATH0():
        import sys
        if opth.split(sys.argv[0])[1].find('py')!=-1:#is script
            return sys.path[0]
        else:return sys.path[1]
    a=HatH(filename='z:\\EHG-579997.hathdl')
    print a.count