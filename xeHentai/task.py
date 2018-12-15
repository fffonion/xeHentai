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
        # self.renamed_map = {} # map fid to renamed file name, used in finding a file by id in RPC

        # original file name only appears in gallery page
        # in single image page it shows a formated image other than original file name

        # file that was in the folder, used to check downloaded files
        # map file name to file size

        # file size check grant more precision in downloaded file check
        self._file_in_download_folder = []

        # map fid to file original name, which appears on gallery pages
        self.fid_2_original_file_name_map = {}

        # map fid to file name, just like the old self.renamed_map
        self.fid_2_file_name_map = {}

        # download range list, former method is too hard to maintain
        self.download_range = []

        # times of image page loading is used by ehentai for counting bandwidth limit
        self.fid_2_file_size_map = {}  # map fid to file size text, reduce image page load

        # and, the fid in these map will all be str
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

            self._file_in_download_folder = []
            self.fid_2_file_size_map = {}
            self.fid_2_original_file_name_map = {}
            self.download_range = []
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

    # write some metadata into zip file
    def encode_meta(self):
        zip_meta = {}
        zip_meta.setdefault('gjname', self.meta['gjname'])
        zip_meta.setdefault('gnname', self.meta['gnname'])
        zip_meta.setdefault('tags', self.meta['tags'])
        zip_meta.setdefault('total', self.meta['total'])
        zip_meta.setdefault('title', self.meta['title'])
        zip_meta.setdefault('rename_ori', self.config['rename_ori'])
        zip_meta.setdefault('download_ori', self.config['download_ori'])
        zip_meta.setdefault('url', self.url)
        zip_meta.setdefault('fid_fname_map', self.fid_2_file_name_map)
        json_zip_meta = json.dumps(zip_meta)
        return ("xeHentai Archiver v%s r1\n%s" % (__version__, json_zip_meta)).encode('UTF-8')

    @staticmethod
    def decode_meta(comment_str):
        _ = re.search('{.+}', comment_str)
        if _:
            meta_str = _[0]
            return json.loads(meta_str)
        else:
            # adapt to older versions
            _ = re.findall('URL:(http.+\/)', comment_str)
            if _:
                metadata = {}
                metadata.setdefault('url', _[0])
                return metadata
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

    @staticmethod
    def get_size_range(size_text):
        _ = re.findall('(\d+(?:\.(\d+))?) *([M|K]?B)', size_text)
        if _:
            _number, _decimal, _unit = _[0]
        else:
            return 0, 0
        number = float(_number)
        uncertain = 0.5

        if _decimal:
            for i in range(0, len(_decimal)):
                uncertain /= 10

        unit = 1
        if _unit == 'KB':
            unit *= 1024
        elif _unit == 'MB':
            unit *= 1048576
        return (number - uncertain) * unit, (number + uncertain) * unit

    def set_reload_url(self, image_url, reload_url, fname, filesize):
        # if same file occurs several times in a gallery
        # to be done with new rename logic

        this_fid = RE_GALLERY.findall(reload_url)[0][1]
        real_file_name = self.fid_2_original_file_name_map[this_fid]

        ext = os.path.splitext(fname)[1]
        if self.config['download_ori']:
            ext = os.path.splitext(real_file_name)[1]

        if not self.config['rename_ori']:
            real_file_name = "%%0%dd%%s" % (len(str(self.meta['total']))) % (int(this_fid), ext)

        if this_fid in self.fid_2_file_name_map:
            self.fid_2_file_name_map[this_fid] = real_file_name
        else:
            self.fid_2_file_name_map.setdefault(this_fid, real_file_name)

        if this_fid not in self.fid_2_file_size_map:
            self.fid_2_file_size_map.setdefault(this_fid, filesize)
        else:
            self.fid_2_file_size_map[this_fid] = filesize

        if image_url in self.reload_map:

            self._f_lock.acquire()
            existed_image_url, existed_file_name = self.reload_map[image_url]
            folder_path = self.get_fpath()
            existed_file = os.path.join(folder_path, existed_file_name)
            new_file = os.path.join(folder_path, real_file_name)
            file_existed = False
            unexpected_file = False
            existed_file_id = RE_GALLERY.findall(existed_image_url)
            if os.path.exists(existed_file):
                file_existed = True
                size_bottom, size_top = self.get_size_range(filesize)
                existed_file_size = os.stat(existed_file).st_size
                if not size_bottom <= existed_file_size < size_top:
                    unexpected_file = True

            if file_existed and not unexpected_file:
                # we can just copy old file if already downloaded
                self._f_lock.acquire()
                try:
                    with open(existed_file, 'rb') as _existed_file:
                        with open(new_file, 'wb') as _new_file:
                            _new_file.write(_existed_file.read())
                except Exception as ex:
                    self._f_lock.release()
                    raise ex
                else:
                    self._f_lock.release()
                    self._cnt_lock.acquire()
                    self.meta['finished'] += 1
                    self._cnt_lock.release()
            elif file_existed and unexpected_file:
                # self._cnt_lock.acquire()
                # self.meta['finished'] -= 1
                # self._cnt_lock.release()
                self.img_q.put(image_url)

            if not file_existed or unexpected_file:
                # if not downloaded, we will copy them in save_file
                if image_url not in self.filehash_map:
                    self.filehash_map[image_url] = []
                self.filehash_map[image_url].append((this_fid, existed_file_id))
                self._f_lock.release()
        else:
            # check file size for downloaded file
            # i would like a hash check
            # but i cant get a hash before downloading the file
            file_existed = False
            unexpected_file = False
            folder_path = self.get_fpath()
            target_file_path = os.path.join(folder_path, real_file_name)
            if os.path.exists(target_file_path):
                file_existed = True
                size_bottom, size_top = self.get_size_range(filesize)
                existed_file_size = os.stat(target_file_path).st_size
                if not size_bottom <= existed_file_size < size_top:
                    unexpected_file = True

            self.reload_map.setdefault(image_url, [reload_url, real_file_name])

            if not file_existed:
                self.img_q.put(image_url)
            elif file_existed and not unexpected_file:
                self._cnt_lock.acquire()
                self.meta['finished'] += 1
                self._cnt_lock.release()
            elif file_existed and unexpected_file:
                # self._cnt_lock.acquire()
                # self.meta['finished'] -= 1
                # self._cnt_lock.release()
                self.img_q.put(image_url)


    def get_reload_url(self, imgurl):
        if not imgurl:
            return
        return self.reload_map[imgurl][0]

    # scan folder or zip file before all worker start working
    # it is designed mainly to remove truncated file and extract those outdated zip files
    def prescan_downloaded(self):
        folder_path = self.get_fpath()

        is_fid_file_name_map_existed = True
        shall_remove_all = False

        will_extract_old_file = False

        metadata = {}
        truncated_img_list = []
        good_img_list = []

        # TODO: in some cases, self.meta['total'] == 0,
        # TODO: this is obviously an error in meta scanning, yet is able to be detected

        # existing of a file doesn't mean the file is correctly downloaded
        # scan zip
        arc = "%s.zip" % folder_path
        if os.path.exists(arc):
            # if the zipfile exists, check the url written in the zipfile
            with zipfile.ZipFile(arc, 'r') as zipfile_target:
                metadata = self.decode_meta(zipfile_target.comment.decode('UTF-8'))
                # check fidmap in the file, if there isn't one, then just renew the zip
                if 'fid_fname_map' not in metadata or not len(metadata['fid_fname_map']) == self.meta['total']:
                    is_fid_file_name_map_existed = False
                # only remove all file when task is download ori but existing file is not
                if 'download_ori' in metadata and not metadata['download_ori'] and self.config['download_ori']:
                    will_extract_old_file = True
                if 'rename_ori' in metadata and not metadata['rename_ori'] == self.config['rename_ori']:
                    will_extract_old_file = True
                if 'url' in metadata and not metadata['url'] == self.url:
                    will_extract_old_file = True

                # when url matches, check every image
                file_name_list = zipfile_target.namelist()
                if is_fid_file_name_map_existed:
                    for _fid, _file_name in metadata['fid_fname_map'].items():
                        if _file_name in file_name_list:
                            zip_info = zipfile_target.getinfo(_file_name)
                            _name, _ext = os.path.splitext(_file_name)
                            if zip_info.file_size == 0 or _ext == '.xeh':
                                truncated_img_list.append(_file_name)
                            elif _ext == '.xehdone':
                                continue
                            else:
                                good_img_list.append(_file_name)
                else:
                    for in_zip_file_name in file_name_list:
                        zip_info = zipfile_target.getinfo(in_zip_file_name)
                        if not zip_info.is_dir():
                            _name, _ext = os.path.splitext(in_zip_file_name)
                            if zip_info.file_size == 0 or _ext == '.xeh':
                                truncated_img_list.append(in_zip_file_name)
                            elif _ext == '.xehdone':
                                continue
                            else:
                                good_img_list.append(in_zip_file_name)

                if len(truncated_img_list) > 0 or not len(good_img_list) == self.meta['total'] \
                        or not is_fid_file_name_map_existed or will_extract_old_file:
                    # extract all image when some images is truncated
                    # or when download is not finished
                    zipfile_target.extractall(folder_path)
                    zipfile_target.close()
                    os.remove(arc)

        # scan xehdone
        elif os.path.exists(os.path.join(folder_path, '.xehdone')):
            # use infomation in .xehdone to check downloaded files
            # just like a extracted zip file
            with open(os.path.join(folder_path, '.xehdone'), 'r') as xehdone:
                comment = xehdone.readline()
                if comment:
                    metadata = self.decode_meta(comment)
            if 'fid_fname_map' not in metadata or not len(metadata['fid_fname_map']) == self.meta['total']:
                is_fid_file_name_map_existed = False

            file_name_list = os.listdir(folder_path)
            if is_fid_file_name_map_existed:
                for _fid, _file_name in metadata['fid_fname_map'].items():
                    if _file_name in file_name_list:
                        _name, _ext = os.path.splitext(_file_name)
                        file_path = os.path.join(folder_path, _file_name)
                        if os.stat(file_path).st_size == 0 or _ext == '.xeh':
                            truncated_img_list.append(_file_name)
                        elif _ext == '.xehdone':
                            continue
                        else:
                            good_img_list.append(_file_name)
            else:
                for file_name in file_name_list:
                    _name, _ext = os.path.splitext(file_name)
                    file_path = os.path.join(folder_path, file_name)
                    if os.stat(file_path).st_size == 0 or _ext == '.xeh':
                        truncated_img_list.append(file_name)
                    elif _ext == '.xehdone':
                        continue
                    else:
                        good_img_list.append(file_name)

        # a zip file properly commented is trustworthy, so program will assume it was completed
        if len(truncated_img_list) == 0 and len(good_img_list) == self.meta['total'] and is_fid_file_name_map_existed\
                and not will_extract_old_file:
            self._flist_done.update(range(1, self.meta['total'] + 1))
            self.fid_2_file_name_map = metadata['fid_fname_map']
        elif len(truncated_img_list) > 0:
            for truncated_img_name in truncated_img_list:
                img_path = os.path.join(folder_path, truncated_img_name)
                os.remove(img_path)

        if not will_extract_old_file:
            self.meta['finished'] = len(self._flist_done)
        if self.meta['finished'] == self.meta['total']:
            return True
        return False

    def scan_downloaded(self, fid_2_page_url_map, scaled = True):
        folder_path = self.get_fpath()
        is_done_file = False
        _range_idx = 0

        scanning_zip = False
        scanning_folder = False

        # scan folder only
        # if there is any problem in zip
        # prescan should have extracted it

        if os.path.exists(folder_path):
            scanning_folder = True
        else:
            for _fid, _page_url in fid_2_page_url_map.items():
                self.page_q.put(_page_url)
            return False

        file_name = ''

        guess_fid_2_file_name_map = {}

        re_name_filter = re.compile('^(\d{%d})\..+$' % len(str(self.meta['total'])))
        self._file_in_download_folder = []

        for _file_name in os.listdir(folder_path):
            _ext = os.path.splitext(_file_name)[1]
            if _ext == '.xeh':
                os.remove(os.path.join(folder_path, _file_name))
            else:
                self._file_in_download_folder.append(_file_name)

        if self.config['rename_ori']:
            for _fid, _file_name in self.fid_2_original_file_name_map.items():
                if os.path.exists(os.path.join(folder_path, _file_name)):
                    guess_fid_2_file_name_map.setdefault(_fid, _file_name)
        else:
            for _file_name in self._file_in_download_folder:
                _ = re_name_filter.findall(_file_name)
                if _:
                    guess_fid_2_file_name_map.setdefault(str(int(_[0])), _file_name)

        for _fid, _url in fid_2_page_url_map.items():
            image_done_file = False
            if _fid in self.fid_2_file_size_map\
                    and _fid in guess_fid_2_file_name_map:
                size_text = self.fid_2_file_size_map[_fid]
                guess_file_name = guess_fid_2_file_name_map[_fid]
                bottom, top = self.get_size_range(size_text)
                size = os.stat(os.path.join(folder_path, guess_file_name)).st_size
                if bottom <= size < top:
                    file_name = guess_file_name
                    image_done_file = True

            if not image_done_file:
                self.page_q.put(_url)
            else:
                if _fid not in self.fid_2_file_name_map:
                    self.fid_2_file_name_map.setdefault(_fid, file_name)
                else:
                    self.fid_2_file_name_map[_fid] = file_name
                self._flist_done.add(int(_fid))

        self.meta['finished'] = len(self._flist_done)
        if self.config['download_range']:
            self.meta['finished'] += (self.meta['total'] - len(self.download_range))
        if self.meta['finished'] == self.meta['total']:
            return True
        return False

    def queue_wrapper(self, callback_page_url_setdefault, pichash = None, img_tuble = None):
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
        # if int(fid) not in self._flist_done:
        #    callback1(img_tuble[0])

        _page_url, _fid, _original_file_name = img_tuble

        if self.config['download_range']:
            if not int(_fid) in self.download_range:
                return

        # if same original name occurs several times
        # this will solve it 
        append_quote = 1
        while True:
            is_crashed = False
            if _fid in self.fid_2_original_file_name_map:
                break
            for fid_in_list, file_name_in_list in self.fid_2_original_file_name_map.items():
                if _original_file_name == file_name_in_list:
                    _file_name, _ext = os.path.splitext(_original_file_name)
                    _original_file_name = '%s_%d%s' % (_file_name, append_quote, _ext)
                    is_crashed = True
                    append_quote += 1
            if not is_crashed:
                break

        if _fid not in self.fid_2_original_file_name_map:
            self.fid_2_original_file_name_map.setdefault(_fid, _original_file_name)
        callback_page_url_setdefault(_fid, _page_url)

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
        # assuming that file is downloaded by other means
        # for example, another instance of xehentai
        # or user just downloaded herself, by dragging from browser

        fname = self.fid_2_file_name_map[fid]

        fn = os.path.join(fpath, fname)
        if os.path.exists(fn):
            os.remove(fn)

        # create a femp file first
        # we don't need _f_lock because this will not be in a sequence
        # and we can't do that otherwise we are breaking the multi threading
        fn_tmp = os.path.join(fpath, ".%s.xeh" % fname)
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
                    fn_rep = os.path.join(fpath, fname)
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
        if fid in self.fid_2_file_name_map:
            ext = os.path.splitext(self.fid_2_file_name_map[fid])[1]
        fid = int(fid)
        _ = "%%0%dd%%s" % (len(str(self.meta['total'])))
        return _ % (fid, ext)

    def make_archive(self, remove=True):
        dpath = self.get_fpath()
        arc = "%s.zip" % dpath
        if os.path.exists(arc):
            # [s]when truncated images not exist, the zip file is considered fully downloaded[\s]
            # [s]but tags still need  update[\s]
            # in fact you can not edit the comment without rezip files, just leave it
            nochange = True
            with zipfile.ZipFile(arc, 'r') as zipfile_target:
                if zipfile_target.comment == self.encode_meta():
                    return arc
                else:
                    zipfile_target.extractall(dpath)

        with zipfile.ZipFile(arc, 'w') as zipfile_target:
            # zip comment created
            # store json info in respective zip file
            # thus metadata can be packed with comic it self in a single file
            zipfile_target.comment = self.encode_meta()
            for _fid, _fname in self.fid_2_file_name_map.items():
                full_path = os.path.join(dpath, _fname)
                zipfile_target.write(full_path, _fname, zipfile.ZIP_STORED)
        if remove:
            self._f_lock.acquire()
            shutil.rmtree(dpath)
            self._f_lock.release()
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