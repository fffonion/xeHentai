# 紳♂士♂站♂小♂爬♂蟲

[![Build Status](https://travis-ci.org/fffonion/xeHentai.svg?branch=master)](https://travis-ci.org/fffonion/xeHentai)

[English](README.md) [简体中文](README.chs.md)

[xeHentai Web界面](https://github.com/fffonion/xeHentai-webui)

## 快速入門

windows用戶可以下載可執行文件 [這裡](https://github.com/fffonion/xeHentai/releases) [或這裡](http://dl.yooooo.us/share/xeHentai/)

或者可以運行源碼

```shell
pip install -U requests[socks]
git clone https://github.com/fffonion/xeHentai.git
cd xeHentai
python ./setup.py install
xeH
```

新版本默認為命令行模式，如果需要使用交互模式，請運行`xeH.py -i`

## 詳細說明

### 配置文件

使用源碼運行的用戶請先將`xeHentai/config.py`複製到當前目錄。

配置的優先級為 交互模式 > 命令行參數 > 用戶config.py > 內置config.py。

常用參數: 

 - **daemon** 後台模式，僅支持posix兼容的系統，參見[運行模式](#運行模式)，默認為否
 - **dir** 下載目錄，默認為當前目錄
 - **download_ori** 是否下載原圖，默認為否
 - **jpn_title** 是否使用日語標題，如果關閉則使用英文或羅馬字標題，默認為是
 - **rename_ori** 將圖片重命名為原始名稱，如果關閉則使用序號，默認為否

高級參數: 

 - **proxy** 代理列表，參見[代理](#代理)。
 - **proxy_image** 是否同時使用代理來下載圖片和掃描網頁，默認為是
 - **proxy_image_only** 是否僅使用代理來下載圖片，不用於掃描網頁，默認為否
 - **rpc_interface** RPC綁定的IP，參見[JSON-RPC](#json-rpc)，默認為`localhost`
 - **rpc_port** RPC綁定的埠，默認為`None`
 - **rpc_secret** RPC密鑰，默認為`None` (不開啟RPC伺服器)
 - **delete_task_files** 是否刪除任務時同時刪除下載的文件，默認為否
 - **make_archive** 是否下載完成後生成zip壓縮包，並刪除下載目錄，默認為否
 - **download_range** 設置下載的圖片範圍，參見[下載範圍](#下載範圍)
 - **scan_thread_cnt** 掃描線程數，默認為`1`
 - **download_thread_cnt** 下載線程數，默認為`5`
 - **download_timeout** 設置下載圖片的超時，默認為`10`秒
 - **ignored_errors** 設置忽略的錯誤碼，默認為空，錯誤碼可以從`const.py`中獲得
 - **log_path** 日誌路徑，默認為`eh.log`
 - **log_verbose** 日誌等級，可選1-3，值越大輸出越詳細，默認為`2`
 - **save_tasks** 是否保存任務到`h.json`，可用於斷點續傳，默認為否
 

### 命令行模式
```
用法: xeH [-u USERNAME] [-k KEY] [-c COOKIE] [-i] [--daemon] [-d DIR] [-o]
           [-j BOOL] [-r BOOL] [-p PROXY] [--proxy-image | --proxy-image-only]
           [--rpc-interface ADDR] [--rpc-port PORT] [--rpc-secret ...]
           [--delete-task-files BOOL] [-a BOOL] [--download-range a-b,c-d,e]
           [-t N] [--timeout N] [-f] [-l /path/to/eh.log] [-v] [-h]
           [--version]
           [url [url ...]]

紳♂士下載器

必選參數:
  url                   下載頁的網址

可選參數:
  -u USERNAME, --username USERNAME
                        用戶名
  -k KEY, --key KEY     密碼
  -c COOKIE, --cookie COOKIE
                        Cookie字符串, 如果指定了用戶名和密碼, 此項會被忽略
  -i, --interactive     交互模式, 如果開啟後台模式, 此項會被忽略 (默認: False)
  --daemon              後台模式 (默認: False)
  -d DIR, --dir DIR     設置下載目錄 (默認: 當前目錄)
  -o, --download-ori    是否下載原始圖片（如果存在）, 需要登錄 (默認: False)
  -j BOOL, --jpn-title BOOL
                        使用日語標題, 如果關閉則使用英文或羅馬字標題 (默認: True)
  -r BOOL, --rename-ori BOOL
                        將圖片重命名為原始名稱, 如果關閉則使用序號 (默認: False)
  -p PROXY, --proxy PROXY
                        設置代理, 可以指定多次, 當前支持的類型: socks5/4a, http(s), glype.
                        代理默認只用於掃描網頁 (默認: 空)
  --proxy-image         同時使用代理來下載圖片和掃描網頁（默認: True)
  --proxy-image-only    僅使用代理來下載圖片, 不用於掃描網頁 (默認: False)
  --rpc-interface ADDR  設置JSON-RPC監聽IP (默認: localhost)
  --rpc-port PORT       設置JSON-RPC監聽埠 (默認: None)
  --rpc-secret ...      設置JSON-RPC密鑰 (默認: None)
  --delete-task-files BOOL
                        刪除任務時同時刪除下載的文件 (默認: False)
  -a BOOL, --archive BOOL
                        下載完成後生成zip壓縮包並刪除下載目錄 (默認: False)
  --download-range a-b,c-d,e
                        設置下載的圖片範圍, 格式為 開始位置-結束位置, 或者單張圖片的位置, 使用逗號來分隔多個範圍, 例如
                        5-10,15,20-25, 默認為下載所有
  -t N, --thread N      下載線程數 (默認: 5)
  --timeout N           設置下載圖片的超時 (默認: 10秒)
  -f, --force           忽略配額判斷, 繼續下載 (默認: False)
  -l /path/to/eh.log, --logpath /path/to/eh.log
                        保存日誌的路徑 (默認: eh.log)
  -v, --verbose         設置日誌裝逼等級 (默認: 2)
  -h, --help            顯示本幫助信息
  --version             顯示版本信息

```

如果參數未指定, 則使用config.py中的默認值；否則將覆蓋config.py設置的值。

### JSON-RPC

在指定`rpc_interface`和`rpc_port`後, xeHentai會啟動RPC伺服器。使用[JSON-RPC 2.0](http://www.jsonrpc.org/specification)標准。典型的請求如下：

```
$ curl localhost:8010/jsonrpc -d '{"jsonrpc": "2.0", "id": 1, "method":"xeH.addTask", "params":[[args],{kwargs}]}'
{"jsonrpc": "2.0", "id": 1, "result": "36df423e"}
```

`rpc_secret`可用於提高安全性。如果`rpc_secret`設置為**hentai**, 則需在params中帶上這個值：
```
$ curl localhost:8010/jsonrpc -d '{"jsonrpc": "2.0", "id": 1, "method":"xeH.addTask", "params":["token:hentai",[args],{kwargs}]}'
{"jsonrpc": "2.0", "id": 1, "result": "36df423e"}
```

其中`method`為調用的方法，必須以**xeH.** 開頭。在[core.py](xeHentai/core.py)的xeHentai類中，所有不以下劃線`_`開頭的方法均可以通過RPC調用，但需將方法名的下劃線命名法改為駝峰命名法。如`add_task`需改為`addTask`。

參數列表請參閱xeHentai類。

如果瀏覽器安裝了用戶腳本插件，可以[下載xeHentaiHelper.user.js](http://dl.yooooo.us/userscripts/xeHentaiHelper.user.js)，將會在頁面上添加`Add to xeHentai`鏈接，以支持將當前頁面添加到xeHentai中。Chrome用戶需要安裝[Tampermonkey](https://chrome.google.com/webstore/detail/tampermonkey/dhdgffkkebhmkfjojejmpbldmpobfkfo)，
Firefox用戶需要安裝[Greasemonkey](https://addons.mozilla.org/en-US/firefox/addon/greasemonkey/)，Opera和傲遊用戶需要安裝暴力猴。

**由於紳士站啟用了https，而rpc走的是http，所以chrome用戶需要點擊地址欄右側盾牌，選擇「加載不安全的腳本」**

### 運行模式

如果通過命令行或交互模式指定了下載url，xeHentai會在下載完成`h.json`中存儲的任務（如果存在）及指定的url後退出。

如果命令行沒有指定url，xeHentai將會在完成存檔`h.json`中的隊列（如果存在）後繼續等待。

如果指定了後台模式（`-d`或設置`daemon`為`True`），xeHentai將會在保持後台運行。

### 代理

目前支持三種模式的代理: 

 - socks代理，如`socks5h://127.0.0.1:1080`；如果需要在客戶端解析DNS，請使用`socks5://127.0.0.1:1080`。
 - http(s)代理，如`http://127.0.0.1:8080`。
 - glype代理，如`http://example.com/browse.php?u=a&b=4`。請根據實際情況修改`b`的名稱。glype是目前使用最廣的php在線代理，使用時請取消勾選「加密url(Encrypt URL)」、取消勾選「移除腳本 (Remove Scripts)」、勾選「允許cookies (Allow Cookies)」後隨意打開一個網頁，然後把網址粘貼進來

可以指定多個代理，格式如`['socks5h://127.0.0.1:1080', 'http://127.0.0.1:8080']`。

默認情況下代理會被用於掃描網頁和下載圖片。如果不需要使用代理下載圖片，請在配置文件中設置`proxy_image`為**False**。

如果使用代理僅用於突破封鎖的目的，則此項可以設置為`False`；如果需要保證隱私，請將此項設置為`True`。使用glype代理的用戶建議將此項設為`False`。

如果僅需要使用代理下載圖片，不需要掃描網頁，請在配置文件中設置`proxy_image_only`為**True**，或者在運行時加上`--proxy-image-only`參數。如果在配置中的`proxy_image`和`proxy_image_only`均為**True**，則`proxy_image`將被忽略。

### 下載範圍

下載範圍的格式為使用`開始位置-結束位置`，例如`5-10`表示下載第5到第10張圖片，包括第5和第10張；或者單個位置，例如`15`表示下載第15張圖片。

可以通過逗號來分割多個範圍，例如`5-10,15`表示下載第5到第10張圖片以及第15張圖片。

如果不輸入下載範圍，則默認下載所有圖片。


## 其他說明

### 配額

直接從伺服器及鏡像途徑下載的圖片計入配額，從H@H下載的不計算；下載新發布的、冷門的漫畫以及原圖更有可能消耗配額，下載熱門漫畫基本不消耗配額

## License

GPLv3
***
![@fffonion](http://img.t.sinajs.cn/t5/style/images/register/logo.png)[@fffonion](http://weibo.com/376463435)&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;![Blog](https://s.w.org/about/images/logos/wordpress-logo-32-blue.png)&nbsp;&nbsp;[博客](https://yooooo.us)
