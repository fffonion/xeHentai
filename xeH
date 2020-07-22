#!/usr/bin/env python

import os
import sys
import json
import zipfile
from threading import Thread
import xeHentai.const as const

SRC_UPDATE_FILE = const.SRC_UPDATE_FILE
if const.PY3K:
    from importlib import reload

def load_update():
    if os.path.exists(SRC_UPDATE_FILE):
        try:
            need_remove = False
            update_id = ""
            with zipfile.ZipFile(SRC_UPDATE_FILE, 'r') as z:
                try:
                   r = json.loads(z.read("info.json"))
                except:
                    need_remove = True
                else:
                    if 'v' not in r and r['v'] != SRC_UPDATE_VERSION:
                        # ignoring legacy file
                        need_remove = True
                    else:
                        update_id = r["update_id"]
            if need_remove:
                os.remove(SRC_UPDATE_FILE)
                return
            v = const.__version__
            sys.path.insert(0, SRC_UPDATE_FILE)
            import xeHentai
            reload(xeHentai)
            xeHentai.const.VERSION_UPDATE = update_id
            xeHentai.const.VERSION_UPDATE_LOADER = v
        except:
            if sys.path[0] == SRC_UPDATE_FILE:
                sys.path.pop(0)
                os.remove(SRC_UPDATE_FILE)


if __name__ == "__main__":
    load_update()

    from xeHentai import cli, i18n
    cli.start()
