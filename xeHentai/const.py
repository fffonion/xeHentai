#!/usr/bin/env python
# coding:utf-8
# constants module
# Contributor:
#      fffonion        <fffonion@gmail.com>

import os
import sys
import locale

PY3K = sys.version_info[0] == 3
IRONPYTHON = sys.platform == 'cli'
EXEBUNDLE = getattr(sys, 'frozen', False)
LOCALE = locale.getdefaultlocale()[0]
CODEPAGE = locale.getdefaultlocale()[1]
ANDROID = 'ANDROID_ARGUMENT' in os.environ

__version__ = 2.005
DEVELOPMENT = True

SCRIPT_NAME = "xeHentai"

# https://github.com/soimort/you-get/you-get
if getattr(sys, 'frozen', False):
    # The application is frozen
    FILEPATH = os.path.dirname(os.path.realpath(sys.executable))
else:
    # The application is not frozen
    # Change this bit to match where you store your data files:
    FILEPATH = sys.path[0]

XEH_STATE_RUNNING = 0
XEH_STATE_SOFT_EXIT = 1 # wait until current task finish and exit
XEH_STATE_FULL_EXIT = 2 # finish current task stage and exit
XEH_STATE_CLEAN = 3

TASK_STATE_PAUSED = 0
TASK_STATE_WAITING = 1
TASK_STATE_GET_META = 2
# TASK_STATE_GET_HATHDL = 3
TASK_STATE_SCAN_PAGE = 3
TASK_STATE_SCAN_IMG = 4
TASK_STATE_SCAN_ARCHIVE = 5
TASK_STATE_DOWNLOAD = 10
TASK_STATE_FINISHED = 20
TASK_STATE_FAILED = -1

ERR_NO_ERROR = 0
ERR_URL_NOT_RECOGNIZED = 1000
ERR_CANT_DOWNLOAD_EXH = 1001
ERR_ONLY_VISIBLE_EXH = 1002
ERR_MALFORMED_HATHDL = 1003
ERR_GALLERY_REMOVED = 1004
ERR_IMAGE_RESAMPLED = 1005
ERR_QUOTA_EXCEEDED = 1006
ERR_KEY_EXPIRED = 1007
ERR_NO_PAGEURL_FOUND = 1008
ERR_TASK_NOT_FOUND = 1101
ERR_SAVE_SESSION_FAILED = 1103
ERR_TASK_LEVEL_UNDEF = 1104
ERR_DELETE_RUNNING_TASK = 1105
ERR_TASK_CANNOT_PAUSE = 1106
ERR_TASK_CANNOT_RESUME = 1107
# ERR_HATHDL_NOTFOUND = 1108
ERR_RPC_UNAUTHORIZED = 1200
ERR_CANNOT_CREATE_DIR = 1300
ERR_RPC_PARSE_ERROR = -32700
ERR_RPC_INVALID_REQUEST = -32600
ERR_RPC_METHOD_NOT_FOUND = -32601
ERR_RPC_INVALID_PARAMS = -32602
ERR_RPC_EXEC_ERROR = -32603
