# coding:utf-8
# Contributor:
#      fffonion        <fffonion@gmail.com>

import requests
import time

from . import Updater, UpdateInfo

class GithubUpdaterException(Exception):
    pass

class GithubUpdater(Updater):
    def __init__(self, session):
        self.session = session

    def get_latest_release(self, dev=False):
        param = dev and "dev" or "master"
        r = self.session.get("https://api.github.com/repos/fffonion/xeHentai/commits?sha=%s" % param)
        commit = r.json()
        if r.status_code != 200 or not commit:
            raise GithubUpdaterException("Failed to get latest release info: %s" % r.text)
        commit = commit[0]
        sha = commit["sha"]
        url = "https://github.com/fffonion/xeHentai/archive/%s.zip" % sha

        return UpdateInfo(
            sha,
            url,
            commit["commit"]["author"]["date"],
            commit["commit"]["message"].replace("\r", " ").replace("\n", " "),
        )

    def get_src_path_in_archive(self, info):
        return "xeHentai-%s/xeHentai" % info.update_id
