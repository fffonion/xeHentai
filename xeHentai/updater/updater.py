# coding:utf-8
# Contributor:
#      fffonion        <fffonion@gmail.com>

import os
import requests
import zipfile
import json
from ..i18n import i18n
from ..util import logger
from ..const import *
from .. import const
from .github import GithubUpdater
from . import UpdateInfo

if PY3K:
    from io import BytesIO as StringIO
else:
    from cStringIO import StringIO 

def check_update(l=None, config={}):
    if not l:
        l = logger.Logger()
    dev = "update_beta_channel" in config and config["update_beta_channel"]
    download_update = "auto_update" in config and config["auto_update"] == "download"
    l.debug(i18n.UPDATE_CHANNEL % (dev and i18n.UPDATE_DEV_CHANNEL or i18n.UPDATE_RELEASE_CHANNEL))
    s = requests.Session()
    g = GithubUpdater(s)
    try:
        info = g.get_latest_release(dev)
        if hasattr(const, "VERSION_UPDATE") and VERSION_UPDATE == info.update_id:
            l.debug(i18n.UPDATE_NO_UPDATE)
            return
        l.info(i18n.UPDATE_AVAILABLE % (info.ts, info.message, info.update_id))
        if not download_update:
            l.info(i18n.UPDATE_DOWNLOAD_MANUALLY)
            return
        resp = s.get(info.download_link)
        z = resp.content
        with zipfile.ZipFile(StringIO(z)) as zf:
            make_src_update_file(zf, g.get_src_path_in_archive(info), info)
        l.info(i18n.UPDATE_COMPLETE)
    except MemoryError as ex:
        l.warn(i18n.UPDATE_FAILED % str(ex))


def make_src_update_file(infile, path, info):
    if not path.endswith("/"):
        path += "/"
    
    with zipfile.ZipFile(SRC_UPDATE_FILE, "w") as z:
        z.writestr(
            "info.json",
            json.dumps({
                "v": SRC_UPDATE_VERSION,
                "update_id": info.update_id,
            }),
            zipfile.ZIP_STORED,
        )

        for f in infile.namelist():
            if f.startswith(path) and not f.endswith("/"):
                z.writestr("xeHentai/%s" % f[len(path):], infile.read(f), zipfile.ZIP_STORED)