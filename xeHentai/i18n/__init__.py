#!/usr/bin/env python
# coding:utf-8
# Contributor:
#      fffonion        <fffonion@gmail.com>

import importlib
from ..const import *
from . import en_us as lng_fallback

try:
    lng = importlib.import_module("%s.i18n.%s" % (SCRIPT_NAME, LOCALE.lower()))
except (ImportError, ValueError):
    lng = lng_fallback


class _(object):
    def c(cls, code):
        return code not in lng.err_msg and \
            (code not in lng_fallback.err_msg and \
                (cls.ERR_NOMSG % code) or \
                    lng_fallback.err_msg[code] ) or \
            lng.err_msg[code]

    def __getattr__(cls, idx):
        return (not hasattr(lng, idx) and \
            getattr(lng_fallback, idx) or \
            getattr(lng, idx)).decode('utf-8')

i18n = _()
