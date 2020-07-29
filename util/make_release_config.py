#coding: utf-8

import os
import sys
import re
FILEPATH = os.path.join(sys.path[0], "..")
sys.path.insert(0, os.path.join(FILEPATH, "xeHentai"))
import config

target = os.path.join(FILEPATH, "release")
if not os.path.exists(target):
    os.mkdir(target)

target = os.path.join(target, "config.py")
cli = open(os.path.join(FILEPATH, "xeHentai", "cli.py"), "rb").read()
zh_hans = open(os.path.join(FILEPATH, "xeHentai", "i18n", "zh_hans.py"), "rb").read()

f = open(target, "wb")
f.write('''# coding:utf-8
# --UTF8补丁-- #

''')

other_mappings = {
        "save_tasks": "是否保存任务到h.json，可用于断点续传",
        "scan_thread_cnt": "扫描线程数",
#        "download_range": "设置下载的图片范围, 格式为 开始位置-结束位置, 或者单张图片的位置\n" + \
#                        "# 使用逗号来分隔多个范围, 例如 5-10,15,20-25, 默认为下载所有"
}

for k in sorted(config.__dict__):
    if k.startswith("__"):
        continue
    if k not in other_mappings:
        if k == "download_range":
            i18n = "XEH_OPT_download_range"
        else:
            i18n = re.findall("_def\[['\"]%s['\"]\].*?help\s*=\s*i18n.([^\)]+)\)" % k, cli, re.DOTALL)[0]
        txt = re.findall("%s\s*=\s*['\"](.*?)\s*\(当前.+['\"]" % i18n, zh_hans, re.DOTALL)[0]
        # multiline fix
        txt = txt.replace('"', '').replace('\\\n', '\n# ')
        txt = re.sub("\nXEH_.+", "", txt, re.DOTALL)
    else:
        txt = other_mappings[k]
    f.write("# %s\n" % txt)
    v = getattr(config, k)
    if isinstance(v, str):
        v = '"%s"' % v
    f.write("%s = %s\n\n" % (k, v))

f.close()
