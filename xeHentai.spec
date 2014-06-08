# -*- mode: python -*-
a = Analysis(['D:\\Dev\\Python\\Workspace\\xeHentai\\xeHentai.py'],
             pathex=['D:\\Dev\\Python\\Workspace\\xeHentai\\', 'D:\\Dev\\Python\\Workspace\\xeHentai\\dependency.zip', 'D:\\Dev\\Python\\Workspace\\xeHentai'],
             hiddenimports=['D:\\Dev\\Python\\Workspace\\xeHentai\\httplib2plus', 'D:\\Dev\\Python\\Workspace\\xeHentai\\convHans', 'D:\\Dev\\Python\\Workspace\\xeHentai\\HatH'],
             hookspath=None,
             runtime_hooks=None)
pyz = PYZ(a.pure)
exe = EXE(pyz,
          a.scripts,
          a.binaries,
          a.zipfiles,
          a.datas,
          name='xeHentai.exe',
          debug=False,
          strip=None,
          upx=True,
          console=True , version='D:\\Dev\\Python\\Workspace\\xeHentai\\verinfo.txt', icon='D:\\Dev\\Python\\Workspace\\xeHentai\\icon3.ico')
