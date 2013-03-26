# -*- mode: python -*-
a = Analysis(['xeHentai-BIG5.py'],
             pathex=['D:\\Dev\\Python\\Workspace\\xeHentai'],
             hiddenimports=[],
             hookspath=None)
pyz = PYZ(a.pure)
exe = EXE(pyz,
          a.scripts,
          a.binaries,
          a.zipfiles,
          a.datas,
          name=os.path.join('dist', 'xeHentai-BIG5.exe'),
          debug=False,
          strip=None,
          upx=True,
          console=True , icon='icon.ico')
