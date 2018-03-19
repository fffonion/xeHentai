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
from threading import RLock
from . import util
from .const import *
from .const import __version__
if PY3K:
    from queue import Queue, Empty
else:
    from Queue import Queue, Empty

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
        self.renamed_map = {} # map fid to renamed file name, used in finding a file by id in RPC
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

    def set_reload_url(self, imgurl, reload_url, fname):
        # if same file occurs severl times in a gallery
        if imgurl in self.reload_map:
            fpath = self.get_fpath()
            old_fid = self.get_fname(imgurl)[0]
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
            self.reload_map[imgurl] = [reload_url, fname]

    def get_reload_url(self, imgurl):
        if not imgurl:
            return
        return self.reload_map[imgurl][0]

    def scan_downloaded(self, scaled = True):
        fpath = self.get_fpath()
        donefile = False
        if os.path.exists(os.path.join(fpath, ".xehdone")) or os.path.exists("%s.zip" % fpath):
            donefile = True
        _range_idx = 0
        for fid in range(1, self.meta['total'] + 1):
            # check download range
            if self.config['download_range']:
                _found = False
                # download_range is sorted asc
                for start, end in self.config['download_range'][_range_idx:]:
                    if fid > end: # out of range right bound move to next range
                        _range_idx += 1
                    elif start <= fid <= end: # in range
                        _found = True
                        break
                    elif fid < start: # out of range left bound
                        break
                if not _found:
                    self._flist_done.add(int(fid))
                    continue
            # can only check un-renamed files
            fname = os.path.join(fpath, self.get_fidpad(fid)) # id
            if donefile:
                self._flist_done.add(int(fid))
            elif os.path.exists(fname):
                if os.stat(fname).st_size == 0:
                    os.remove(fname)
                else:
                    self._flist_done.add(int(fid))
        self.meta['finished'] = len(self._flist_done)
        if self.meta['finished'] == self.meta['total']:
            self.state == TASK_STATE_FINISHED

    def queue_wrapper(self, callback, pichash = None, url = None):
        # if url is not finished, call callback to put into queue
        # type 1: normal file; type 2: resampled url
        # if pichash:
        #     fid = int(self.meta['filelist'][pichash][0])
        #     if fid not in self._flist_done:
        #         callback(self.get_picpage_url(pichash))
        # elif url:
        fhash, fid = RE_GALLERY.findall(url)[0]
        # if fhash not in self.meta['filelist']:
        #     self.meta['resampled'][fhash] = int(fid)
        #     self.has_ori = True]
        if int(fid) not in self._flist_done:
            callback(url)

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

        fn = os.path.join(fpath, self.get_fidpad(int(fid)))
        if os.path.exists(fn) and os.stat(fn).st_size > 0:
            return fn
        # create a femp file first
        # we don't need _f_lock because this will not be in a sequence
        # and we can't do that otherwise we are breaking the multi threading
        fn_tmp = os.path.join(fpath, ".%s.xeh" % self.get_fidpad(int(fid)))
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

    def get_fidpad(self, fid, ext = 'jpg'):
        fid = int(fid)
        _ = "%%0%dd.%%s" % (len(str(self.meta['total'])))
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
            fname_ori = os.path.join(fpath, self.get_fidpad(fid)) # id          
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
                fname_to = os.path.join(fpath, self.get_fidpad(fid, os.path.splitext(fname)[1][1:]))
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
                            _base = re.sub("\((\d+)\)$", _base, lambda x:"(%d)" % (int(x.group(1)) + 1))
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
            return arc
        with zipfile.ZipFile(arc, 'w')  as zipFile:
            zipFile.comment = ("xeHentai Archiver v%s\nTitle:%s\nOriginal URL:%s" % (
                __version__, self.meta['title'], self.url)).encode('utf-8')
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
