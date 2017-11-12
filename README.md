# E-Hentai Dōjinshi Downloader

[![Build Status](https://travis-ci.org/fffonion/xeHentai.svg?branch=master)](https://travis-ci.org/fffonion/xeHentai)

[简体中文](README.chs.md) [繁體中文](README.cht.md)

[xeHentai WebUI](https://github.com/fffonion/xeHentai-webui)

## TL;DR

Windows users can download packed binaries from [here](https://github.com/fffonion/xeHentai/releases) or [here](http://dl.yooooo.us/share/xeHentai/). The package is built using [PyInstaller](http://www.pyinstaller.org/).

Or run directly from source code:

```shell
pip install -U requests[socks]
git clone https://github.com/fffonion/xeHentai.git
cd xeHentai
python ./setup.py install
xeH
```

The program is running in non-interactive mode by default. To run interactively, use `xeH.py -i`.

## For pros

### Configuration file

If you are running from source code, please copy `xeHentai/config.py` to your current directory first. Use that file as your config file.

The priority of configuration is: Interactive inputs > Command line options > User config.py > Internal config.py.

Configuration keys：

 - **daemon** Set to run in default mode, can only use on posix-compatible systems. Refer to [Running Modes](#running-modes). Default to `False`.
 - **dir** Download directory. Default to current directory.
 - **download_ori** Set to download original images or not. Default to `False`.
 - **jpn_title** Set to select Japanese title or not. If set to `False`, English or Romaji title will be used. Default to `True`.
 - **rename_ori** Set to rename images to their orginal names. If set to `False`, image will be named in sequence numbers. Default to `False`.

 - **proxy** Proxy list. Refer to [Proxies](#proxies).
 - **proxy_image** Set to use proxy both on downloading images and scanning webpages. Default to `True`.
 - **proxy_image_only** Set to use proxy only on downloading images. Default to `False`.
 - **rpc_interface** RPC server binding IP. Refer to [JSON-RPC](#json-rpc). Default to `localhost`.
 - **rpc_port** RPC server binding port. Default to `none` (not serving).
 - **rpc_secret** RPC secret key. Default to `None`.
 - **delete_task_files** Set to delete downloaded files when deleting a task. Default to `False`.
 - **make_archive** Set to make a ZIP archive after download and delete downloaded directory. Default to `False`.
 - **download_range** Set image download range. Refer to [Download range](#download-range). Default to download all images.
 - **scan_thread_cnt** Thread count for scanning webpages. Default to `1`.
 - **download_thread_cnt** Thread count for downloading images. Default to `5`.
 - **download_timeout** Timeout of download images. Default to `10`s.
 - **ignored_errors** Set the error codes to ignore and continue downloading. Default to *empty*. Error codes can be obtained from [const.py](xeHentai/const.py).
 - **log_path** Set log file path. Default to `eh.log`.
 - **log_verbose** Set log level with integer from 1 to 3. Bigger value means more verbose output. Default to `2`.
 - **save_tasks** Set to save uncompleted tasks in `h.json`. Default to `False`.
 

### Command line options
```
Usage: xeH [-u USERNAME] [-k KEY] [-c COOKIE] [-i] [--daemon] [-d DIR] [-o]
           [-j BOOL] [-r BOOL] [-p PROXY] [--proxy-image | --proxy-image-only]
           [--rpc-interface ADDR] [--rpc-port PORT] [--rpc-secret ...]
           [--delete-task-files BOOL] [-a BOOL] [--download-range a-b,c-d,e]
           [-t N] [--timeout N] [-f] [-l /path/to/eh.log] [-v] [-h]
           [--version]
           [url [url ...]]

xeHentai Downloader NG

positional arguments:
  url                   gallery url(s) to download

optional arguments:
  -u USERNAME, --username USERNAME
                        username
  -k KEY, --key KEY     password
  -c COOKIE, --cookie COOKIE
                        cookie string, will be overriden if given -u and -k
  -i, --interactive     interactive mode, will be ignored in daemon mode
                        (default: False)
  --daemon              daemon mode, can't use with -i (default: False)
  -d DIR, --dir DIR     set download directory (current:
                        /Users/fffonion/Dev/Python/xeHentai)
  -o, --download-ori    download original images, needs to login (current:
                        True)
  -j BOOL, --jpn-title BOOL
                        use Japanese title, use English/Romaji title if turned
                        off (default: True)
  -r BOOL, --rename-ori BOOL
                        rename gallery image to original name, use sequence
                        name if turned off (default: False)
  -p PROXY, --proxy PROXY
                        set download proxies, can be used multiple times,
                        currenlty supported: socks5/4a, http(s), glype.
                        Proxies are only used on webpages by default (current:
                        ['socks5h://127.0.0.1:16963'])
  --proxy-image         use proxies on images and webpages (default: True)
  --proxy-image-only    only use proxies on images, not webpages (current:
                        False)
  --rpc-interface ADDR  bind jsonrpc server to this address (current:
                        localhost)
  --rpc-port PORT       bind jsonrpc server to this port (default: 8010)
  --rpc-secret ...      jsonrpc secret string (default: None)
  --delete-task-files BOOL
                        delete downloaded files when deleting a task (default:
                        True)
  -a BOOL, --archive BOOL
                        make an archive (.zip) after download and delete
                        directory (default: False)
  --download-range a-b,c-d,e
                        specify ranges of images to be downloaded, in format
                        start-end, or single index, use comma to concat
                        multiple ranges, e.g.: 5-10,15,20-25, default to
                        download all images
  -t N, --thread N      download threads count (default: 5)
  --timeout N           set image download timeout (default: 10s)
  -f, --force           download regardless of quota exceeded warning
                        (default: False)
  -l /path/to/eh.log, --logpath /path/to/eh.log
                        define log path (current:
                        /Users/fffonion/Dev/Python/xeHentai/eh.log)
  -v, --verbose         show more detailed log (default: 3)
  -h, --help            show this help message and exit
  --version             show program's version number and exit

```

If options are not defined, values from `config.py` will be used.

### JSON-RPC

If `rpc_interface` and `rpc_port` are set, xeHentai will start a RPC server. The request and response follows the [JSON-RPC 2.0](http://www.jsonrpc.org/specification) standard.

```
$ curl localhost:8010/jsonrpc -d '{"jsonrpc": "2.0", "id": 1, "method":"xeH.addTask", "params":[[args],{kwargs}]}'
{"jsonrpc": "2.0", "id": 1, "result": "36df423e"}
```

`rpc_secret` is a secret key to your RPC server. If it's set, client should include this value in the request. For example when `rpc_secret` is set to **hentai**: 
```
$ curl localhost:8010/jsonrpc -d '{"jsonrpc": "2.0", "id": 1, "method":"xeH.addTask", "params":["token:hentai",[args],{kwargs}]}'
{"jsonrpc": "2.0", "id": 1, "result": "36df423e"}
```

The method filed should start with **xeH.** and should be a public class method of **xeHentai** from [core.py](xeHentai/core.py). And change the name from *lower_case_with_underscores* notation to *lowerCamelCase* notation. For example, `add_task` becomes `addTask`.

Refer to **xeHentai** class from [core.py](xeHentai/core.py) for parameters list.

If your browser has a Userscript plugin, you can use [xeHentaiHelper.user.js](http://dl.yooooo.us/userscripts/xeHentaiHelper.user.js) to create tasks directly on e-hentai website. Chrome user will need to install [Tampermonkey](https://chrome.google.com/webstore/detail/tampermonkey/dhdgffkkebhmkfjojejmpbldmpobfkfo), for firefox [Greasemonkey](https://addons.mozilla.org/en-US/firefox/addon/greasemonkey/), and ViolentMonkey for Opera and Maxthon users.

**Because e-hentai has enabled https, Chrome user will needs to click on the shield icon in the far right of the address bar and click "Load anyway" or "Load unsafe scripts"**

### Running modes

If xeHentai is ran from command line interface or interative mode, the program will exit after it finishes the tasks in `h.json` (if exists) and given URL.

If there's no URL given from command line, the program will exit after it finishes the tasks in `h.json`(if exists).

If program is running on daemon mode (`-d` is set or `daemon` is set to `True`), the program will keep running in background.

### Proxies

xeHentai supports three types of proxies:

 - socks proxy: `socks5h://127.0.0.1:1080`. If you want to resolve DNS on client side, use `socks5://127.0.0.1:1080`.
 - http(s) proxy: `http://127.0.0.1:8080`.
 - glype proxy: `http://example.com/browse.php?u=a&b=4`. Please set value of `b` accordingly. glype is a widely used PHP proxy script. When using, uncheck **Encrypt URL**, **Remove Scripts** and check **Allow Cookies** and open a random URL. The paste the address into configuration.

Multiple proxies can be specified at the same time. The format can be like : `['socks5h://127.0.0.1:1080', 'http://127.0.0.1:8080']`. 

By default proxies are used to download images and scan webpages. If you don't want to use proxy on downloading images, set `proxy_image` to `False`.

glype users are encouraged to set `proxy_image` to `False`。

If you only want to use proxy to download image, set `proxy_image_only` to **True** in `config.py` or use the `--proxy-image-only` CLI option. If both `proxy_image` and `proxy_image_only` are set to **True**, `proxy_image` will be ignored.

### Download range

Download ranges are set in format `start_positoin-end_positoin`. For example, `5-10` means number download first 5 to 10 images, including 5 and 10. Or use `15` to download number 15 only.

Multiple ranges can be seperated with comma. For example,`5-10,15`.

If no range is given, xeHentai will download all images.


## Misc

### Image limit

Downloading images will be count towards image limit. This is calculated regarding the popularity of gallery, the server load and/or Hentai@Home bandwidth by e-hentai server.

## License

GPLv3
***
![Blog](https://s.w.org/about/images/logos/wordpress-logo-32-blue.png)&nbsp;&nbsp;[Blog](https://yooooo.us)
