import re
a='/g/browse.php?u=http%3A%2F%2Fehgt.org%2Fg%2F509s.gif&amp;b=4'
arg='u'
def FIX_REDIRECT(str):
    url=re.findall(arg+'=(.*?)&',str)
    print url
FIX_REDIRECT(a)