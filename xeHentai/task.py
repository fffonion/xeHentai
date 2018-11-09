#!/usr/bin/env python
# coding:utf-8
# Contributor:
#      fffonion        <fffonion@gmail.com>

import os
import re
import copy
import json
import uuid
import shutil
import zipfile

import glob

from threading import RLock
from . import util
from .const import *
from .const import __version__
if PY3K:
    from queue import Queue, Empty
else:
    from Queue import Queue, Empty

from PIL import ImageFile

class Task(object):
    def __init__(self, url, cfgdict):
        self.url = url
        if url:
            _ = RE_INDEX.findall(url)
            if _:
                self.gid, self.sethash = _[0]
        self.failcode = 0
        self.state = TASK_STATE_WAITING
        self.guid = str(uuid.uuid4())[:8]
        self.config = cfgdict
        self.meta = {}
        self.has_ori = False
        self.reload_map = {} # {url:reload_url}
        self.filehash_map = {} # map same hash to different ids, {url:((id, fname), )}

        # renamed map just don't work well with extension part
        
        # this situation happens especially with animated galleries
        # in which you would download an original file even you dont use the Download original link
        # renamed map still thinks the .gif file is a .jpg file
        # thus you can only view the first frame of the gif

        # when downloading original file, renamed map still choose the extension in single image page
        # you will get an png file renamed to be a jpg file
        # well, in fact you can view the file just fine
        # but photoshop says "no, png file have to be a png file"

        # besides, in some rare cases, the original png file is so small
        # that you will get an original png file when not download original
        
        # it is somehow hard to upgrade the old method
        # i choose to write a new one
        self.renamed_map = {} # map fid to renamed file name, used in finding a file by id in RPC
       
        # original file name only apears in gallery page
        # in single image page it shows an formarted image other than original file name

        # file that was in the folder, used to check downloaded files
        # map file name to file size
        # file size check grant more precision in downloaded file check
        self._file_in_download_folder = {}
        # map fid to file original name, which appears on gallery pages
        self.original_fname_map = {}
        # map fid to file name, just like the old self.renamed_map
        self.fid_fname_map = {}
        # aaaand, the fid in these map will all be str
        # when int key dumps into files by python, it is somehow transformed into str
        # and an error would occur when you load it again 


        self.img_q = None
        self.page_q = None
        self.list_q = None
        self._flist_done = set() # store id, don't save, will generate when scan
        self._monitor = None
        self._cnt_lock = RLock()
        self._f_lock = RLock()

    def cleanup(self, before_delete=False):
        if before_delete:
            if 'delete_task_files' in self.config and self.config['delete_task_files'] and \
                'title' in self.meta: # maybe it's a error task and meta is empty
                fpath = self.get_fpath()
                # TODO: ascii can't decode? locale not enus, also check save_file
                if os.path.exists(fpath):
                    shutil.rmtree(fpath)
                zippath = "%s.zip" % fpath
                if os.path.exists(zippath):
                    os.remove(zippath)
        elif self.state in (TASK_STATE_FINISHED, TASK_STATE_FAILED):
            self.img_q = None
            self.page_q = None
            self.list_q = None
            self.reload_map = {}
        
            # if 'filelist' in self.meta:
            #     del self.meta['filelist']
            # if 'resampled' in self.meta:
            #     del self.meta['resampled']

    def set_fail(self, code):
        self.state = TASK_STATE_FAILED
        self.failcode = code
        # cleanup all we cached
        self.meta = {}

    def migrate_exhentai(self):
        _ = re.findall("(?:https*://[g\.]*e\-hentai\.org)(.+)", self.url)
        if not _:
            return False
        self.url = "https://exhentai.org%s" % _[0]
        self.state = TASK_STATE_WAITING if self.state == TASK_STATE_FAILED else self.state
        self.failcode = 0
        return True

    #write some metadata into zip file
    def encodeMetaForZipComment(self):
        zipmeta = {}
        zipmeta.setdefault('gjname',self.meta['gjname'])
        zipmeta.setdefault('gnname',self.meta['gnname'])
        zipmeta.setdefault('tags',self.meta['tags'])
        zipmeta.setdefault('total',self.meta['total'])
        zipmeta.setdefault('title',self.meta['title'])
        zipmeta.setdefault('rename_ori',self.config['rename_ori'])
        zipmeta.setdefault('download_ori',self.config['download_ori'])
        zipmeta.setdefault('url',self.url)
        zipmeta.setdefault('fid_fname_map',self.fid_fname_map)
        jsonzipmeta = json.dumps(zipmeta);
        return ("xeHentai Archiver v%s r1\n%s" % ( __version__, jsonzipmeta)).encode('UTF-8')

    def decodeMetaFromZipComment(self,comment):
        comment_str = comment.decode('UTF-8')
        _ = re.search('{.+}',comment_str)
        if _:
            metaStr = _[0]
            return json.loads(metaStr)
        else:
            #adapt to older versions
            _ = re.findall('URL:(http.+\/)',comment_str)
            if _:
                metadata = {}
                metadata.setdefault('url',_[0])
                return metadata;
            else:
                return {}

    def update_meta(self, meta):
        self.meta.update(meta)
        if self.config['jpn_title'] and self.meta['gjname']:
            self.meta['title'] = self.meta['gjname']
        else:
            self.meta['title'] = self.meta['gnname']

    # def guess_ori(self):
    #     # guess if this gallery has resampled files depending on some sample hashes
    #     # return True if it's ori
    #     if 'sample_hash' not in self.meta:
    #         return
    #     all_keys = map(lambda x:x[:10], self.meta['filelist'].keys())
    #     for h in self.meta['sample_hash']:
    #         if h not in all_keys:
    #             self.has_ori = True
    #             break
    #     del self.meta['sample_hash']

    def base_url(self):
        return re.findall(RESTR_SITE, self.url)[0]

    # def get_picpage_url(self, pichash):
    #     # if file resized, this url not works
    #     # http://%s.org/s/hash_s/gid-picid'
    #     return "%s/s/%s/%s-%s" % (
    #         self.base_url(), pichash[:10], self.gid, self.meta['filelist'][pichash][0]
    #     )

    def set_reload_url(self, imgurl, reload_url, fname, filesize):
        # if same file occurs severl times in a gallery
        # to be done with new rename logic

        if imgurl in self.reload_map:
            fpath = self.get_fpath()
            old_fid = self.get_fname(imgurl)[0]

            if not old_fid in real_file_name_map.keys():
                file_name_map.setdefault(old_fid,fname)

            old_f = os.path.join(fpath, self.get_fidpad(old_fid))
            this_fid = int(RE_GALLERY.findall(reload_url)[0][1])
            this_f = os.path.join(fpath, self.get_fidpad(this_fid))
            self._f_lock.acquire()
            if os.path.exists(old_f):
                # we can just copy old file if already downloaded
                try:
                    with open(old_f, 'rb') as _of:
                        with open(this_f, 'wb') as _nf:
                            _nf.write(_of.read())
                except Exception as ex:
                    self._f_lock.release()
                    raise ex
                else:
                    self._f_lock.release()
                    self._cnt_lock.acquire()
                    self.meta['finished'] += 1
                    self._cnt_lock.release()
            else:
                # if not downloaded, we will copy them in save_file
                if imgurl not in self.filehash_map:
                    self.filehash_map[imgurl] = []
                self.filehash_map[imgurl].append((this_fid, old_fid))
                self._f_lock.release()
        else:
            this_fid = RE_GALLERY.findall(reload_url)[0][1]
            realfname = self.original_fname_map[this_fid]

            ext = os.path.splitext(fname)[1]
            if self.config['download_ori']:
                ext = os.path.splitext(realfname)[1]

            if not self.config['rename_ori']:
                realfname = "%%0%dd%%s" % (len(str(self.meta['total']))) % (int(this_fid),ext)
            self.fid_fname_map.setdefault(this_fid,realfname)
            
            # check file size for downloaded file
            # i would like a hash check
            # but i cant get a hash before downloading the file
            file_existed = False
            unexpected_file = False
            if realfname in self._file_in_download_folder:
                file_existed = True
                fpath = self.get_fpath()
                fsize = self._file_in_download_folder[realfname]
                float_size = float(fsize)/1024
                size_unit = 'KB'
                if float_size > 1024:
                    float_size = float_size / 1024
                    size_unit = 'MB'
                if float_size > 100:
                    size_text = '%.1f %s' % ( float_size, size_unit)
                else:
                    size_text = '%.2f %s' % ( float_size, size_unit)
                if not size_text == filesize:
                    unexpected_file = True

            if not file_existed:
                self.img_q.put(imgurl)
            elif unexpected_file:
                self.meta['finished'] -= 1
                self.img_q.put(imgurl)

            self.reload_map[imgurl] = [reload_url, realfname]


    def get_reload_url(self, imgurl, fid):
        if not imgurl:
            return
        return self.reload_map[imgurl][0]

    def scan_downloaded_zipfile(self):
        fpath = self.get_fpath()

        donefile = False
        isOutdated = False
        removeAll = False

        if os.path.exists(os.path.join(fpath, ".xehdone")):
            donefile = True
        metadata = {}
        #existing of a file doesn't mean the file is corectly downloaded
        arc = "%s.zip" % fpath
        if os.path.exists(arc):
            #if the zipfile exists, check the url written in the zipfile
            with zipfile.ZipFile(arc,'r') as zipfileTarget:
                metadata = self.decodeMetaFromZipComment(zipfileTarget.comment)
                #check fidmap in the file, if there isn't one, then just renew the zip
                if 'fid_fname_map' in metadata:
                    fid_map = metadata['fid_fname_map']
                else:
                   isOutdated = True

                if 'download_ori' in metadata:
                    if not metadata['download_ori'] == self.config['download_ori']:
                        removeAll = True
                        isTruncated = True

                if 'rename_ori' in metadata:
                    if not metadata['rename_ori'] == self.config['rename_ori']:
                        removeAll = True
                        isTruncated = True

                if not isOutdated and 'url' in metadata and metadata['url'] == self.url:
                    #when url matches, check every image
                    
                    filenameList = zipfileTarget.namelist()
                    isTruncated = False
                    truncated_img_list = []
                    goodimgList = []
                    for in_zip_file_name in filenameList:
                        zipinfo = zipfileTarget.getinfo(in_zip_file_name)
                        if not zipinfo.is_dir():
                            ext = os.path.splitext(in_zip_file_name)
                            if ext == '.xehdone':
                                continue
                            elif ext == '.xeh':
                                isTruncated = True
                                truncated_img_list.append(in_zip_file_name)
                            elif removeAll:
                                truncated_img_list.append(in_zip_file_name)
                            else:
                                try:
                                    fp = zipfileTarget.open(in_zip_file_name)
                                    imagep = ImageFile.Parser()
                                    while True:
                                        buffer = fp.read(1024)
                                        if not buffer:
                                            break
                                        imagep.feed(buffer)
                                    image = imagep.close()
                                    fp.close()
                                    image.close()
                                    goodimgList.append(in_zip_file_name)
                                except IOError:
                                    isTruncated = True
                                    fp.close()
                                    truncated_img_list.append(in_zip_file_name)
                                    continue
                    if not len(goodimgList) == len(fid_map):
                        isTruncated = True

                    if isTruncated:
                        #extract all image when some images is truncated
                        #and remove those truncated file
                        zipfileTarget.extractall(fpath)
                        zipfileTarget.close()
                        os.remove(arc)
                        for truncated_img_name in truncated_img_list:
                            imgpath = os.path.join(fpath,truncated_img_name)
                            os.remove(imgpath)
                    elif not len(self.fid_fname_map) == metadata['total']:
                        #not finished
                        zipfileTarget.extractall(fpath)
                    else:
                        donefile = True
                elif isOutdated:
                    zipfileTarget.extractall(fpath)
        
        #a zip file properly commented is trustworth, so program will assume it was completed
        if donefile:
            self._flist_done.update(range(1, self.meta['total'] + 1))
        self.meta['finished'] = len(self._flist_done)
        if self.meta['finished'] == self.meta['total']:
            self.state == TASK_STATE_FINISHED

    def scan_downloaded(self, scaled = True):
        fpath = self.get_fpath()
        donefile = False
        if os.path.exists(os.path.join(fpath, ".xehdone")):
            donefile = True
        _range_idx = 0

        for root,dirs,files in os.walk(fpath):
            for file_name_in_fpath in files:
                si_fpath = os.path.join(fpath,file_name_in_fpath)
                self._file_in_download_folder.setdefault(file_name_in_fpath,os.stat(si_fpath).st_size)

        if self.config['rename_ori']:
            return

        fidlist = {}
        totaldigit = len(str(self.meta['total']))
        nametemplate = '[0-9]'
        for i in range(1,totaldigit):
            nametemplate = '%s%s' %(nametemplate,'[0-9]')

        filelist = glob.glob(os.path.join(glob.escape(fpath),'%s%s' % (nametemplate,'.*')))

        for filename in filelist:
            fname,ext = os.path.splitext(os.path.basename(filename))
            #sometimes the archiver archived some unfinished files
            if not ext == '.xeh':
                fidlist.setdefault(int(fname), filename)

        for fid in range(1, self.meta['total'] + 1):
            # check download range
            #if self.config['download_range']:
            #    _found = False
                # download_range is sorted asc
            #    for start, end in self.config['download_range'][_range_idx:]:
            #        if fid > end: # out of range right bound move to next range
            #            _range_idx += 1
            #       elif start <= fid <= end: # in range
            #            _found = True
            #            break
            #        elif fid < start: # out of range left bound
            #            break
            #    if not _found:
            #        self._flist_done.add(int(fid))
            #        continue
            # can only check un-renamed files
            #fname = os.path.join(fpath, self.get_fidpad("%d" % fid)) # id
            
            if donefile:
                self._flist_done.add(int(fid))
            elif fid in fidlist.keys():
                fname = fidlist[fid]
                if os.stat(fname).st_size == 0:
                    os.remove(fname)
                else:
                    self._flist_done.add(int(fid))
                    self.fid_fname_map.setdefault('%d' % fid, os.path.basename(fname))
        self.meta['finished'] = len(self._flist_done)
        if self.meta['finished'] == self.meta['total']:
            self.state == TASK_STATE_FINISHED

    def queue_wrapper(self, pichash = None, img_tuble = None):
        # if url is not finished, call callback to put into queue
        # type 1: normal file; type 2: resampled url
        # if pichash:
        #     fid = int(self.meta['filelist'][pichash][0])
        #     if fid not in self._flist_done:
        #         callback(self.get_picpage_url(pichash))
        # elif url:
        fhash, fid = RE_GALLERY.findall(img_tuble[0])[0]

        # if fhash not in self.meta['filelist']:
        #     self.meta['resampled'][fhash] = int(fid)
        #     self.has_ori = True]
        #if int(fid) not in self._flist_done:
        #    callback1(img_tuble[0])
        self.original_fname_map.setdefault(img_tuble[1],img_tuble[2])
        self.page_q.put(img_tuble[0])

    def save_file(self, imgurl, redirect_url, binary_iter):
        # TODO: Rlock for finished += 1
        fpath = self.get_fpath()
        self._f_lock.acquire()
        if not os.path.exists(fpath):
            os.mkdir(fpath)
        self._f_lock.release()
        pageurl, fname = self.reload_map[imgurl]
        _ = re.findall("/([^/\?]+)(?:\?|$)", redirect_url)
        if _: # change it if it's a full image
            fname = _[0]
            self.reload_map[imgurl][1] = fname
        _, fid = RE_GALLERY.findall(pageurl)[0]

        # if a same file exists
        # assumimg that file is downloaded by other means
        # for example, another instance of xehentai
        # or user just downloaded herself, by dragging from browser
        ext = os.path.splitext(fname)[1]
        fn = os.path.join(fpath, self.get_fidpad(fid,ext))
        if os.path.exists(fn) and os.stat(fn).st_size > 0:
            self._cnt_lock.acquire()
            self.meta['finished'] += 1
            self._cnt_lock.release()
            return True

        # create a femp file first
        # we don't need _f_lock because this will not be in a sequence
        # and we can't do that otherwise we are breaking the multi threading
        fn_tmp = os.path.join(fpath, ".%s.xeh" % self.get_fidpad(fid,ext))
        try:
            with open(fn_tmp, "wb") as f:
                for binary in binary_iter():
                    if self._monitor._exit(None):
                        raise DownloadAbortedException()
                    f.write(binary)
        except DownloadAbortedException as ex:
            os.remove(fn_tmp)
            return

        self._f_lock.acquire()
        try:
            os.rename(fn_tmp, fn)
            self._cnt_lock.acquire()
            self.meta['finished'] += 1
            self._cnt_lock.release()
            if imgurl in self.filehash_map:
                for _fid, _ in self.filehash_map[imgurl]:
                    # if a file download is interrupted, it will appear in self.filehash_map as well
                    if _fid == int(fid):
                        continue
                    fn_rep = os.path.join(fpath, self.get_fidpad(_fid))
                    shutil.copyfile(fn, fn_rep)
                    self._cnt_lock.acquire()
                    self.meta['finished'] += 1
                    self._cnt_lock.release()
                del self.filehash_map[imgurl]
        except Exception as ex:
            self._f_lock.release()
            raise ex
        self._f_lock.release()
        return True

    def get_fname(self, imgurl):
        pageurl, fname = self.reload_map[imgurl]
        _, fid = RE_GALLERY.findall(pageurl)[0]
        return int(fid), fname

    def get_fpath(self):
        return os.path.join(self.config['dir'], util.legalpath(self.meta['title']))

    def get_fidpad(self, fid, ext = '.jpg'):
        if fid in self.fid_fname_map:
            ext = os.path.splitext(self.fid_fname_map[fid])[1]
        fid = int(fid)
        _ = "%%0%dd%%s" % (len(str(self.meta['total'])))
        return _ % (fid, ext)

    def rename_fname(self):
        fpath = self.get_fpath()
        tmppath = os.path.join(fpath, RENAME_TMPDIR)
        cnt = 0
        error_list = []
        # we need to track renamed fid's to decide 
        # whether to rename into a temp filename or add (1)
        # only need it when rename_ori = True
        done_list = set()
        for h in self.reload_map:
            fid, fname = self.get_fname(h)
            # if we don't need to rename to original name and file type matches
            if not self.config['rename_ori'] and os.path.splitext(fname)[1].lower() == '.jpg':
                continue
            fname_ori = os.path.join(fpath, self.get_fidpad("%d" % fid)) # id          
            if self.config['rename_ori']:
                if os.path.exists(os.path.join(tmppath, self.get_fidpad(fid))):
                    # if we previously put it into a temporary folder, we need to change fname_ori
                    fname_ori = os.path.join(tmppath, self.get_fidpad(fid))
                fname_to = os.path.join(fpath, util.legalpath(fname))
            else:
                # Q: Why we don't just use id.ext when saving files instead of using
                #   id.jpg?
                # A: If former task doesn't download all files, a new task with same gallery
                #   will have zero knowledge about file type before scanning all per page,
                #   thus can't determine if this id is downloaded, because file type is not
                #   necessarily .jpg
                fname_to = os.path.join(fpath, self.get_fidpad("%d" % fid, os.path.splitext(fname)[1][1:]))
            while fname_ori != fname_to:
                if os.path.exists(fname_ori):
                    while os.path.exists(fname_to):
                        _base, _ext = os.path.splitext(fname_to)
                        _ = re.findall("\((\d+)\)$", _base)
                        if self.config['rename_ori'] and fname_to not in done_list:
                            # if our auto numbering conflicts with original naming
                            # we move it into a temporary folder
                            # It's safe since this file is same with one of our auto numbering filename,
                            # it could never be conflicted with other files in tmppath
                            if not os.path.exists(tmppath):
                                os.mkdir(tmppath)
                            os.rename(fname_to, os.path.join(tmppath, os.path.split(fname_to)[1]))
                            break
                        if _ :# if ...(1) exists, use ...(2)
                            print(_base)
                            #_base = re.sub("\((\d+)\)$", _base, lambda x:"(%d)" % (int(x.group(1)) + 1))
                            _base = re.sub("\((\d+)\)$", _base, lambda x:"(%d)" % (int(x.group(0)) + 1))
                        else:
                            _base = "%s(1)" % _base
                        fname_to = "".join((_base, _ext))
                try:
                    os.rename(fname_ori, fname_to)
                    self.renamed_map[str(fid)] = os.path.split(fname_to)[1]
                except Exception as ex:
                    error_list.append((os.path.split(fname_ori)[1], os.path.split(fname_to)[1], str(ex)))
                    break
                if self.config['rename_ori']:
                    done_list.add(fname_to)
                break
            cnt += 1
        if cnt == self.meta['total']:
            with open(os.path.join(fpath, ".xehdone"), "w"):
                pass
        try:
            os.rmdir(tmppath)
        except: # we will leave it undeleted if it's not empty
            pass
        return error_list

    def make_archive(self, remove=True):
        dpath = self.get_fpath()
        arc = "%s.zip" % dpath
        if os.path.exists(arc):
            #when truncated images not exist, the zip file is considered fully downloaded
            #but tags still need  update
            nochange = True
            with zipfile.ZipFile(arc, 'r') as zipfileTarget:
                if zipfileTarget.comment == self.encodeMetaForZipComment():
                    return arc

        with zipfile.ZipFile(arc, 'w')  as zipFile:
            #zip comment created
            #zipFile.comment = ("xeHentai Archiver v%s customized by Dynilath\n%s" % ( __version__, jsonzipmeta)).encode('utf-8')
            #store json info in respective zip file
            #thus metadata can be packed with comic it self in a single file
            zipFile.comment = self.encodeMetaForZipComment()
            for f in sorted(os.listdir(dpath)):
                fullpath = os.path.join(dpath, f)
                zipFile.write(fullpath, f, zipfile.ZIP_STORED)
        if remove:
            shutil.rmtree(dpath)
        return arc

    def from_dict(self, j):
        for k in self.__dict__:
            if k not in j:
                continue
            if k.endswith('_q') and j[k]:
                setattr(self, k, Queue())
                [getattr(self, k).put(e, False) for e in j[k]]
            else:
                setattr(self, k, j[k])
        _ = RE_INDEX.findall(self.url)
        if _:
            self.gid, self.sethash = _[0]
        return self


    def to_dict(self):
        d = dict({k:v for k, v in self.__dict__.items()
            if not k.endswith('_q') and not k.startswith("_")})
        for k in ['img_q', 'page_q', 'list_q']:
            if getattr(self, k):
                d[k] = [e for e in getattr(self, k).queue]
        return d