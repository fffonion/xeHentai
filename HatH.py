#!/usr/bin/python2.7
# -*- coding:utf-8 -*-
# A file parser for h@h(.hathdl)
# Contributor:
#      fffonion        <fffonion@gmail.com>

__version__=1.1
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
            self._name=self.htmlescape(re.findall('TITLE (.+)',self.content)[0])
            self._gid=int(re.findall('GID (.+)',self.content)[0])
            self._total_count=int(re.findall('FILES (.+)',self.content)[0])
            self._title=re.findall('Title:\s+(.+)',self.content)[0]
            self._upload_time=re.findall('Upload Time:\s+(.+)',self.content)[0]#invisible
            self._upload_by=re.findall('Uploaded By:\s+(.+)',self.content)[0]#invisible
            self._downloaded=re.findall('Downloaded:\s+(.+)',self.content)[0]#invisible
            self._tags=re.findall('Tags:\s+(.+)',self.content)[0].split(', ')
            self._listtmp=re.findall('FILELIST\n(.+)\n+\nINFORMATION',self.content,re.DOTALL)[0]
            self._listtmp=self._listtmp.split('\n')
        except IndexError,ValueError:
            print('File or content illegal.')
        self.setpath(dirpath)
        self.genlist(self._listtmp,check)
        
    def htmlescape(self,str):
        def replc(match):
            #print match.group(0),match.group(1),match.group(2)
            dict={'amp':'&','nbsp':' ','quot':'"','lt':'<','gt':'>','copy':'©','reg':'®'}
			#dict+={'∀':'forall','∂':'part','∃':'exist','∅':'empty','∇':'nabla','∈':'isin','∉':'notin','∋':'ni','∏':'prod','∑':'sum','−':'minus','∗':'lowast','√':'radic','∝':'prop','∞':'infin','∠':'ang','∧':'and','∨':'or','∩':'cap','∪':'cup','∫':'int','∴':'there4','∼':'sim','≅':'cong','≈':'asymp','≠':'ne','≡':'equiv','≤':'le','≥':'ge','⊂':'sub','⊃':'sup','⊄':'nsub','⊆':'sube','⊇':'supe','⊕':'oplus','⊗':'otimes','⊥':'perp','⋅':'sdot','Α':'Alpha','Β':'Beta','Γ':'Gamma','Δ':'Delta','Ε':'Epsilon','Ζ':'Zeta','Η':'Eta','Θ':'Theta','Ι':'Iota','Κ':'Kappa','Λ':'Lambda','Μ':'Mu','Ν':'Nu','Ξ':'Xi','Ο':'Omicron','Π':'Pi','Ρ':'Rho','Σ':'Sigma','Τ':'Tau','Υ':'Upsilon','Φ':'Phi','Χ':'Chi','Ψ':'Psi','Ω':'Omega','α':'alpha','β':'beta','γ':'gamma','δ':'delta','ε':'epsilon','ζ':'zeta','η':'eta','θ':'theta','ι':'iota','κ':'kappa','λ':'lambda','μ':'mu','ν':'nu','ξ':'xi','ο':'omicron','π':'pi','ρ':'rho','ς':'sigmaf','σ':'sigma','τ':'tau','υ':'upsilon','φ':'phi','χ':'chi','ψ':'psi','ω':'omega','ϑ':'thetasym','ϒ':'upsih','ϖ':'piv','Œ':'OElig','œ':'oelig','Š':'Scaron','š':'scaron','Ÿ':'Yuml','ƒ':'fnof','ˆ':'circ','˜':'tilde',' ':'ensp',' ':'emsp',' ':'thinsp','‌':'zwnj','‍':'zwj','‎':'lrm','‏':'rlm','–':'ndash','—':'mdash','‘':'lsquo','’':'rsquo','‚':'sbquo','“':'ldquo','”':'rdquo','„':'bdquo','†':'dagger','‡':'Dagger','•':'bull','…':'hellip','‰':'permil','′':'prime','″':'Prime','‹':'lsaquo','›':'rsaquo','‾':'oline','€':'euro','™':'trade','←':'larr','↑':'uarr','→':'rarr','↓':'darr','↔':'harr','↵':'crarr','⌈':'lceil','⌉':'rceil','⌊':'lfloor','⌋':'rfloor','◊':'loz','♠':'spades','♣':'clubs','♥':'hearts','♦':'diams'}
            if match.groups>2:
                if match.group(1)=='#':
                    return unichr(int(match.group(2)))
                else:
                    return  dict.get(match.group(2),'?')
        htmlre=re.compile("&(#?)(\d{1,5}|\w{1,8}|[a-z]+);")
        return htmlre.sub(replc,str)
    
    def setpath(self,path):
        if path:
            setattr(self,'dirpath',path)
        else:
            setattr(self,'dirpath',self._name.decode('utf-8'))
        self.genlist(self._listtmp,self.check)
        
    def genlist(self,raw_list,check=True):
        #generate full list
        self._piclist=[self.picelem(raw_list[i],self.gid) for i in range(len(raw_list))]
        #scan folder for former pics
        self._remainindex=self._checkexist(self.dirpath,check)
        #re-generate
        self._piclist_veryfied=[self._piclist[i] for i in self._remainindex]
        self._count=len(self._piclist_veryfied)
    
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
        for i in range(self._total_count):
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
        for i in range(self._total_count):
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
        if key=='count':
            return self._count
        elif key=='total':
            return self._total_count
        elif key=='gid':
            return self._gid
        elif key=='name':
            return self._name
        elif key=='title':
            return self._title
        elif key=='tags':
            return self._tags
        elif key=='list':
            return self._piclist_veryfied
        else:
            return getattr(self,key)
        

if __name__=='__main__':
    def getPATH0():
        import sys
        if opth.split(sys.argv[0])[1].find('py')!=-1:#is script
            return sys.path[0]
        else:return sys.path[1]
    a=HatH(filename='z:\\EHG-579997.hathdl')
    print a.count