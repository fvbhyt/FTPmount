"""挂载点完整链路验收：读写改名建目录删目录，每步与服务器核对。"""
import hashlib
import os
import subprocess
import sys

R = "R:\\"
FTP = "ftp://127.0.0.1:2121/"
ok = 0
fail = 0


def check(name, cond, detail=""):
    global ok, fail
    if cond:
        ok += 1
        print("  [OK]   %s %s" % (name, detail))
    else:
        fail += 1
        print("  [FAIL] %s %s" % (name, detail))


def server_list(path=""):
    out = subprocess.run(
        ["curl", "-s", "-u", "test:test", FTP + path],
        capture_output=True,
    ).stdout.decode("utf-8", "replace")
    return out


def server_get(path):
    return subprocess.run(
        ["curl", "-s", "-u", "test:test", FTP + path],
        capture_output=True,
    ).stdout


print("1) 新建并写入")
p = os.path.join(R, "acc.txt")
with open(p, "w", encoding="utf-8") as f:
    f.write("acceptance-line-1\n")
check("服务器出现文件", "acc.txt" in server_list())
check("服务器内容一致", server_get("acc.txt") == b"acceptance-line-1\n")

print("2) 覆盖写入（变长）")
with open(p, "w", encoding="utf-8") as f:
    f.write("short\n")
check("长度变为 6", os.path.getsize(p) == 6, "实际 %d" % os.path.getsize(p))
check("服务器内容一致", server_get("acc.txt") == b"short\n")

print("3) 追加写入")
with open(p, "a", encoding="utf-8") as f:
    f.write("tail\n")
check("长度变为 11", os.path.getsize(p) == 11, "实际 %d" % os.path.getsize(p))
check("服务器内容一致", server_get("acc.txt") == b"short\ntail\n")

print("4) 改名")
p2 = os.path.join(R, "acc2.txt")
os.rename(p, p2)
check("新名字在服务器上", "acc2.txt" in server_list())
check("旧名字已消失", "acc.txt" not in server_list())
check("改名后内容保留", server_get("acc2.txt") == b"short\ntail\n")

print("5) 建目录 + 目录内写文件")
d = os.path.join(R, "accdir")
os.mkdir(d)
check("服务器出现目录", "accdir" in server_list())
inner = os.path.join(d, "in.txt")
with open(inner, "w", encoding="utf-8") as f:
    f.write("nested\n")
check("服务器目录内出现文件", "in.txt" in server_list("accdir/"))
check("目录内文件内容一致", server_get("accdir/in.txt") == b"nested\n")

print("6) 读回校验（含二进制一致性）")
h_local = hashlib.md5(open(inner, "rb").read()).hexdigest()
h_srv = hashlib.md5(server_get("accdir/in.txt")).hexdigest()
check("md5 一致", h_local == h_srv, "%s" % h_local)

print("7) 删除空目录")
empty = os.path.join(R, "accempty")
os.mkdir(empty)
os.rmdir(empty)
check("服务器上已删除", "accempty" not in server_list())

print("8) 删除非空目录应报错")
try:
    os.rmdir(d)
    check("非空目录被拒绝", False, "竟然成功了")
except OSError as e:
    check("非空目录被拒绝", True, str(e.strerror))

print()
print("通过 %d 项，失败 %d 项" % (ok, fail))
sys.exit(1 if fail else 0)
