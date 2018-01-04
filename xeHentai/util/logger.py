#!/usr/bin/env python
# coding:utf-8
# Contributor:
#      fffonion        <fffonion@gmail.com>

import os
import sys
import datetime
import locale
import logging
from threading import RLock
#import logging.handlers
from ..const import *

class tz_GMT8(datetime.tzinfo):
    def utcoffset(self, dt):
        return datetime.timedelta(hours = 8)
    def dst(self, dt):
        return datetime.timedelta(0)

def safestr(s):
    if (PY3K and isinstance(s, bytes)) or (not PY3K and not isinstance(s, unicode)):
        s = s.decode("utf-8")
    if PY3K:
        return s
    return s.encode(locale.getdefaultlocale()[1] or 'utf-8', 'replace')
    #return _.decode('utf-8') if PY3K else _

if os.name == 'nt':
    endl = '\r\n'
else:# assume posix
    endl = '\n'

class Logger(object):
    # paste from goagent
    CRITICAL = 5
    FATAL = CRITICAL
    ERROR = 4
    WARNING = 3
    WARN = WARNING
    INFO = 2
    DEBUG = 1
    VERBOSE = 0
    def __init__(self, *args, **kwargs):
        # self.level = self.__class__.INFO
        self.logf = None
        self.__write = __write = lambda x: sys.stdout.write(safestr(x))
        self.isatty = getattr(sys.stdout, 'isatty', lambda: False)()
        self.__set_error_color = lambda: None
        self.__set_warning_color = lambda: None
        self.__set_debug_color = lambda: None
        self.__set_verbose_color = lambda: None
        self.__reset_color = lambda: None
        if self.isatty:
            if os.name == 'nt':
                self._nt_color_lock = RLock()
                import ctypes
                SetConsoleTextAttribute = ctypes.windll.kernel32.SetConsoleTextAttribute
                GetStdHandle = ctypes.windll.kernel32.GetStdHandle
                self.__set_error_color = lambda: (self._nt_color_lock.acquire(), SetConsoleTextAttribute(GetStdHandle(-11), 0x0C))
                self.__set_warning_color = lambda: (self._nt_color_lock.acquire(), SetConsoleTextAttribute(GetStdHandle(-11), 0x06))
                self.__set_debug_color = lambda: (self._nt_color_lock.acquire(), SetConsoleTextAttribute(GetStdHandle(-11), 0x02))
                self.__set_verbose_color = lambda: (self._nt_color_lock.acquire(), SetConsoleTextAttribute(GetStdHandle(-11), 0x08))
                self.__set_bright_color = lambda: (self._nt_color_lock.acquire(), SetConsoleTextAttribute(GetStdHandle(-11), 0x0F))
                self.__reset_color = lambda: (SetConsoleTextAttribute(GetStdHandle(-11), 0x07), self._nt_color_lock.release())
            elif os.name == 'posix':
                self.__set_error_color = lambda: __write('\033[31m')
                self.__set_warning_color = lambda: __write('\033[33m')
                self.__set_debug_color = lambda: __write('\033[32m')
                self.__set_verbose_color = lambda: __write('\033[36m')
                self.__set_bright_color = lambda: __write('\033[32m')
                self.__reset_color = lambda: __write('\033[0m')


    @classmethod
    def getLogger(cls, *args, **kwargs):
        return cls(*args, **kwargs)

    def cleanup(self):
        if self.logf:
            _ = self.logf
            self.logf = None
            _.close()

    def set_logfile(self, fpath):
        if self.logf:
            self.logf.close()
        self.logf = open(fpath, "ab")

    def set_level(self, level):
        f = ('verbose', 'debug', 'info')
        lv = min(max(level, 0), 3)
        for p in range(lv):
            setattr(self, f[p], self.dummy)

    def log(self, level, fmt, *args, **kwargs):
        # fmt=du8(fmt)
        try:
            try:
                self.__write('%-4s - [%s] %s\n' % (level, datetime.datetime.now(tz_GMT8()).strftime('%X'), fmt % args))
            except (ValueError, TypeError):
                fmt = fmt.replace('%','%%')
                self.__write('%-4s - [%s] %s\n' % (level, datetime.datetime.now(tz_GMT8()).strftime('%X'), fmt % args))
        except IOError: # fix for Windows console
            pass
        sys.stdout.flush()
        if self.logf:
            _ = ('[%s] %s%s' % (datetime.datetime.now(tz_GMT8()).strftime('%b %d %X'), fmt % args, endl))
            self.logf.write(_.encode("utf-8", 'replace'))

    def dummy(self, *args, **kwargs):
        pass

    def debug(self, fmt, *args, **kwargs):
        self.__set_debug_color()
        self.log('DEBG', fmt, *args, **kwargs)
        self.__reset_color()

    def info(self, fmt, *args, **kwargs):
        puretext = self.log('INFO', fmt, *args)
        # if self.logfile:
        #    self.logfile.write(puretext)

    def verbose(self, fmt, *args, **kwargs):
        self.__set_verbose_color()
        self.log('VERB', fmt, *args, **kwargs)
        self.__reset_color()

    def warning(self, fmt, *args, **kwargs):
        self.__set_warning_color()
        self.log('WARN', fmt, *args, **kwargs)
        self.__reset_color()

    def warn(self, fmt, *args, **kwargs):
        self.warning(fmt, *args, **kwargs)

    def error(self, fmt, *args, **kwargs):
        self.__set_error_color()
        self.log('ERROR', fmt, *args, **kwargs)
        self.__reset_color()

    def exception(self, fmt, *args, **kwargs):
        self.error(fmt, *args, **kwargs)
        traceback.print_exc(file = sys.stderr)

    def critical(self, fmt, *args, **kwargs):
        self.__set_error_color()
        self.log('CRITICAL', fmt, *args, **kwargs)
        self.__reset_color()
