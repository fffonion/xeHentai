# -*- mode: python -*-
a = Analysis(['eHentaiBot.py'],
             pathex=['D:\\Dev\\Python\\Workspace\\ehentai'],
             hiddenimports=[],
             hookspath=None)
pyz = PYZ(a.pure)
exe = EXE(pyz,
          a.scripts,
          a.binaries,
          a.zipfiles,
          a.datas,
          name=os.path.join('dist', 'eHentai.exe'),
          debug=False,
          strip=None,
          upx=True,
          console=True , icon='icon.ico')
