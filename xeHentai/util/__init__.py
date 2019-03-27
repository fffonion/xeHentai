#!/usr/bin/env python
# coding:utf-8
# Contributor:
#      fffonion        <fffonion@gmail.com>

import os
import re
import sys
import uuid
import random

from ..const import *

if os.name == 'nt':
    filename_filter = re.compile("[|:?\\/*'\"<>]|\.+(?:$)")
else:# assume posix
    filename_filter = re.compile("[\/:]")

if PY3K:
    unichr = chr

def parse_cookie(coostr):
    ret = {}
    for coo in coostr.split(";"):
        coo = coo.strip()
        if coo.lower() in ('secure', 'httponly'):
            continue
        _ = coo.split("=")
        k = _[0]
        v = "=".join(_[1:])
        if k.lower() in ('path', 'expires', 'domain', 'max-age', 'comment'):
            continue
        ret[k] = v
    return ret

def make_cookie(coodict):
    return ";".join(map("=".join, coodict.items()))

def make_ua():
    rrange = lambda a, b, c = 1: c == 1 and random.randrange(a, b) or int(1.0 * random.randrange(a * c, b * c) / c)
    ua = 'Mozilla/%d.0 (Windows NT %d.%d) AppleWebKit/%d (KHTML, like Gecko) Chrome/%d.%d Safari/%d' % (
        rrange(4, 7, 10), rrange(5, 7), rrange(0, 3), rrange(535, 538, 10),
        rrange(21, 27, 10), rrange(0, 9999, 10), rrange(535, 538, 10)
    )

def get_proxy_policy(cfg):
    if cfg['proxy_image_only']:
        return RE_URL_IMAGE
    if cfg['proxy_image']:
        return RE_URL_ALL
    return RE_URL_WEBPAGE

def parse_human_time(s):
    rt = 0
    day = re.findall('(\d+)\sdays*', s)
    if day:
        rt += 86400 * int(day[0])
    hour = re.findall('(\d+)\shours*', s)
    if hour:
        rt += 3600 * int(hour[0])
    minute = re.findall('(\d+)\sminutes*', s)
    if minute:
        rt += 60 * int(minute[0])
    else:
        rt += 60
    return rt

def htmlescape(s):
    def replc(match):
        #print match.group(0),match.group(1),match.group(2)
        dict={'amp':'&','nbsp':' ','quot':'"','lt':'<','gt':'>','copy':'©','reg':'®'}
        #dict+={'∀':'forall','∂':'part','∃':'exist','∅':'empty','∇':'nabla','∈':'isin','∉':'notin','∋':'ni','∏':'prod','∑':'sum','−':'minus','∗':'lowast','√':'radic','∝':'prop','∞':'infin','∠':'ang','∧':'and','∨':'or','∩':'cap','∪':'cup','∫':'int','∴':'there4','∼':'sim','≅':'cong','≈':'asymp','≠':'ne','≡':'equiv','≤':'le','≥':'ge','⊂':'sub','⊃':'sup','⊄':'nsub','⊆':'sube','⊇':'supe','⊕':'oplus','⊗':'otimes','⊥':'perp','⋅':'sdot','Α':'Alpha','Β':'Beta','Γ':'Gamma','Δ':'Delta','Ε':'Epsilon','Ζ':'Zeta','Η':'Eta','Θ':'Theta','Ι':'Iota','Κ':'Kappa','Λ':'Lambda','Μ':'Mu','Ν':'Nu','Ξ':'Xi','Ο':'Omicron','Π':'Pi','Ρ':'Rho','Σ':'Sigma','Τ':'Tau','Υ':'Upsilon','Φ':'Phi','Χ':'Chi','Ψ':'Psi','Ω':'Omega','α':'alpha','β':'beta','γ':'gamma','δ':'delta','ε':'epsilon','ζ':'zeta','η':'eta','θ':'theta','ι':'iota','κ':'kappa','λ':'lambda','μ':'mu','ν':'nu','ξ':'xi','ο':'omicron','π':'pi','ρ':'rho','ς':'sigmaf','σ':'sigma','τ':'tau','υ':'upsilon','φ':'phi','χ':'chi','ψ':'psi','ω':'omega','ϑ':'thetasym','ϒ':'upsih','ϖ':'piv','Œ':'OElig','œ':'oelig','Š':'Scaron','š':'scaron','Ÿ':'Yuml','ƒ':'fnof','ˆ':'circ','˜':'tilde',' ':'ensp',' ':'emsp',' ':'thinsp','‌':'zwnj','‍':'zwj','‎':'lrm','‏':'rlm','–':'ndash','—':'mdash','‘':'lsquo','’':'rsquo','‚':'sbquo','“':'ldquo','”':'rdquo','„':'bdquo','†':'dagger','‡':'Dagger','•':'bull','…':'hellip','‰':'permil','′':'prime','″':'Prime','‹':'lsaquo','›':'rsaquo','‾':'oline','€':'euro','™':'trade','←':'larr','↑':'uarr','→':'rarr','↓':'darr','↔':'harr','↵':'crarr','⌈':'lceil','⌉':'rceil','⌊':'lfloor','⌋':'rfloor','◊':'loz','♠':'spades','♣':'clubs','♥':'hearts','♦':'diams'}
        if len(match.groups()) >= 2:
            if match.group(1) == '#':
                return unichr(int(match.group(2)))
            else:
                return dict.get(match.group(2), '?')
    htmlre = re.compile("&(#?)(\d{1,5}|\w{1,8}|[a-z]+);")
    return htmlre.sub(replc, s)

def legalpath(s):
    sanitized = filename_filter.sub(lambda x:"", s)
    # windows doesn't like trailing while spaces
    if os.name == 'nt':
        sanitized = sanitized.rstrip()
    return sanitized
