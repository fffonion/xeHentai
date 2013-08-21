#coding:utf-8
from distutils.core import setup
import py2exe
import sys
 
#this allows to run it with a simple double click.
sys.argv.append('py2exe')
sys.path.append('dependency')
py2exe_options = {
        "includes": ["HatH","httplib2plus","convHans"],
		"excludes":['_gtkagg', '_tkagg', 'bsddb', 'curses', 'pywin.debugger','pywin.debugger.dbgcon', 'pywin.dialogs','tcl','Tkconstants', 'Tkinter','doctest','pdb','unittest','difflib','inspect','optparse', 'pickle','_ssl'],
		"dll_excludes": ["MSVCP90.dll","msvcr71.dll"],
        "compressed": 1,
        "optimize": 2,
        "ascii": 0,
        "bundle_files": 1,
		"dist_dir":'release'
        }
 
setup(
      name = 'xeHentai',
      #version = '1.5.4.5',
	  #description='Éð¡áÊ¿Âþ»­ÏÂÔØÆ÷',
	  #author='fffonion',
      console = [{"script":'xeHentai.py',"icon_resources":[(1, "icon3.ico")]}],
      zipfile = None,
      options = {'py2exe': py2exe_options}
      )
 