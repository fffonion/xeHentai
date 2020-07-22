# coding:utf-8
# Contributor:
#      fffonion        <fffonion@gmail.com>

class Updater(object):
    def get_latest_release(self, dev=False):
        raise NotImplementedError("get_latest_release not implemented")

    def get_src_path_in_archive(self, info):
        raise NotImplementedError("get_src_path_in_archive not implemented")

class UpdateInfo(object):
    def __init__(self, update_id, download_link, ts, message):
        self.update_id = update_id
        self.download_link = download_link
        self.message = message
        self.ts = ts

