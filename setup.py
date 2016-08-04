#!/usr/bin/env python

PROJ_NAME = 'xeHentai'
PACKAGE_NAME = 'xeHentai'

PROJ_METADATA = '%s.json' % PROJ_NAME

import os, json, imp
here = os.path.abspath(os.path.dirname(__file__))

try:
    README = open(os.path.join(here, 'README.md')).read()
except:
    README = ""
try:
    CHANGELOG = open(os.path.join(here, 'CHANGELOG.md')).read()
except:
    CHANGELOG = ""
VERSION = imp.load_source('version', os.path.join(here, '%s/const.py' % PACKAGE_NAME)).__version__

packages = [
    'xeHentai',
    'xeHentai.util',
    'xeHentai.i18n',
]
requires = ['requests']

from setuptools import setup

setup(
    name=PACKAGE_NAME,
    version=VERSION,
    description='xeHentai Downloader',
    long_description=README + '\n\n' + CHANGELOG,
    author='fffonion',
    author_email='fffonion@gmail.com',
    url='https://yooooo.us/2013/xehentai',
    packages=packages,
    package_dir={'requests': 'requests'},
    include_package_data=True,
    install_requires=requires,
    license='GPLv3',
    zip_safe=False,
    classifiers=(
        'Development Status :: 4 - Beta',
        'Intended Audience :: End Users/Desktop',
        'Natural Language :: English',
        'License :: OSI Approved :: GPLv3',
        'Programming Language :: Python',
        'Programming Language :: Python :: 2.6',
        'Programming Language :: Python :: 2.7',
        'Programming Language :: Python :: 3',
        'Programming Language :: Python :: 3.3',
        'Programming Language :: Python :: 3.4',
        'Programming Language :: Python :: 3.5',
        'Programming Language :: Python :: Implementation :: CPython',
        'Programming Language :: Python :: Implementation :: PyPy'
    ),
    requires=requires,
    entry_points = {'console_scripts': ["xeH = xeHentai.cli:start"]},
)