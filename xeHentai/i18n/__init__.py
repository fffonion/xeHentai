#!/usr/bin/env python
# coding:utf-8
# Contributor:
#      fffonion        <fffonion@gmail.com>

import importlib
from ..const import *
from . import en_us as lng_fallback

try:
    _locale = LOCALE.lower() if LOCALE else 'en_us'
    if _locale in ('zh_cn', 'zh_sg'):
        _locale = 'zh_hans'
    elif _locale in ('zh_tw', 'zh_hk', 'zh_mo'):
        _locale = 'zh_hant'
    lng = importlib.import_module("%s.i18n.%s" % (SCRIPT_NAME, _locale))
except (ImportError, ValueError):
    lng = lng_fallback


class _(object):
    def c(cls, code):
        _ = code not in lng.err_msg and \
            (code not in lng_fallback.err_msg and \
                (cls.ERR_NOMSG % code) or \
                    lng_fallback.err_msg[code] ) or \
            lng.err_msg[code]
        return _ if PY3K else (
            _ if isinstance(_, unicode) else _.decode('utf-8')) # cls.ERR_NOMSG % code is unicode

    def __getattr__(cls, idx):
        _ = not hasattr(lng, idx) and \
            getattr(lng_fallback, idx) or \
            getattr(lng, idx)
        return _ if PY3K else _.decode('utf-8')

i18n = _()
