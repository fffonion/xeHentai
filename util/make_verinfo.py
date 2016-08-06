# coding:utf-8

import os
import sys
sys.path.insert(0, os.path.join(sys.path[0], ".."))
from xeHentai import const

version = const.__version__
v = list(str(int(version*1000)))
tmpl='''# UTF-8
#
# For more details about fixed file info 'ffi' see:
# http://msdn.microsoft.com/en-us/library/ms646997.aspx
VSVersionInfo(
  ffi=FixedFileInfo(
    # filevers and prodvers should be always a tuple with four items: (1, 2, 3, 4)
    # Set not needed items to zero 0.
    filevers=(%s),
    prodvers=(%s),
    # Contains a bitmask that specifies the valid bits 'flags'r
    mask=0x3f,
    # Contains a bitmask that specifies the Boolean attributes of the file.
    flags=0x0,
    # The operating system for which this file was designed.
    # 0x4 - NT and there is no need to change it.
    OS=0x40004,
    # The general type of file.
    # 0x1 - the file is an application.
    fileType=0x1,
    # The function of the file.
    # 0x0 - the function is not defined for this fileType
    subtype=0x0,
    # Creation date and time stamp.
    date=(0, 0)
    ),
  kids=[
    StringFileInfo(
      [
      StringTable(
        u'080404B0',
        [StringStruct(u'FileVersion', u'%s'),
        StringStruct(u'ProductVersion', u'%s'),
        StringStruct(u'OriginalFilename', u'xeHentai-%s.exe'),
        StringStruct(u'InternalName', u'xeHentai'),
        StringStruct(u'FileDescription', u'绅♂士漫画下载器'),
        StringStruct(u'CompanyName', u'fffonion@gmail.com'),
        StringStruct(u'LegalCopyright', u'GPLv3'),
        StringStruct(u'ProductName', u'xeHentai')])
      ]), 
    VarFileInfo([VarStruct(u'Translation', [2052, 1200])])
  ]
)''' % (
", ".join(v), ", ".join(v), 
".".join(v), ".".join(v), version
)

open("verinfo.txt", "wb").write(tmpl)
