# -*- coding: utf-8 -*-
"""验证挂载盘上的 create/overwrite 语义（对应 WinFsp 的 CreateDisposition）。

关键断言：
  1. CREATE_NEW 打已存在文件 -> 必须失败 ERROR_FILE_EXISTS(80)
     这就是资源管理器弹「替换或跳过文件」的触发条件。
  2. CREATE_ALWAYS 打已存在文件 -> 必须成功（内核走 Open + Overwrite），内容变成新的。
  3. OPEN_ALWAYS 打已存在文件 -> 必须成功，且**内容不能被截断**（惰性打开不能丢旧内容）。
  4. OPEN_EXISTING 后随机位置改写 -> 未写到的区间必须保留旧内容（惰性补齐要生效）。
"""
import ctypes
import sys
from ctypes import wintypes

k32 = ctypes.WinDLL("kernel32", use_last_error=True)

GENERIC_READ = 0x80000000
GENERIC_WRITE = 0x40000000
CREATE_NEW = 1
CREATE_ALWAYS = 2
OPEN_EXISTING = 3
OPEN_ALWAYS = 4
FILE_ATTRIBUTE_NORMAL = 0x80
INVALID_HANDLE_VALUE = wintypes.HANDLE(-1).value

k32.CreateFileW.restype = wintypes.HANDLE
k32.CreateFileW.argtypes = [
    wintypes.LPCWSTR, wintypes.DWORD, wintypes.DWORD, ctypes.c_void_p,
    wintypes.DWORD, wintypes.DWORD, wintypes.HANDLE,
]
k32.WriteFile.argtypes = [wintypes.HANDLE, ctypes.c_char_p, wintypes.DWORD,
                          ctypes.POINTER(wintypes.DWORD), ctypes.c_void_p]
k32.ReadFile.argtypes = [wintypes.HANDLE, ctypes.c_char_p, wintypes.DWORD,
                         ctypes.POINTER(wintypes.DWORD), ctypes.c_void_p]
k32.SetFilePointer.argtypes = [wintypes.HANDLE, wintypes.LONG, ctypes.c_void_p, wintypes.DWORD]
k32.SetEndOfFile.argtypes = [wintypes.HANDLE]
k32.CloseHandle.argtypes = [wintypes.HANDLE]


def open_(path, access, disp):
    ctypes.set_last_error(0)
    h = k32.CreateFileW(path, access, 0, None, disp, FILE_ATTRIBUTE_NORMAL, None)
    return h, ctypes.get_last_error()


def ok(h):
    return h not in (INVALID_HANDLE_VALUE, None, -1)


def write_all(h, data):
    n = wintypes.DWORD(0)
    k32.WriteFile(h, data, len(data), ctypes.byref(n), None)
    return n.value


def read_all(path):
    h, err = open_(path, GENERIC_READ, OPEN_EXISTING)
    if not ok(h):
        return None, err
    buf = ctypes.create_string_buffer(1 << 20)
    n = wintypes.DWORD(0)
    k32.ReadFile(h, buf, len(buf), ctypes.byref(n), None)
    k32.CloseHandle(h)
    return buf.raw[: n.value], 0


def main():
    base = sys.argv[1] if len(sys.argv) > 1 else r"Y:\_wb_x"
    p = base + r"\t_ov.txt"
    fails = 0

    def check(name, cond, detail=""):
        nonlocal fails
        if cond:
            print(f"  [OK]   {name}")
        else:
            fails += 1
            print(f"  [FAIL] {name}  {detail}")

    # 准备：写一个 OLD
    h, err = open_(p, GENERIC_WRITE, CREATE_ALWAYS)
    if not ok(h):
        print(f"准备阶段打不开 {p}，err={err}")
        return 2
    write_all(h, b"OLD")
    k32.CloseHandle(h)
    got, _ = read_all(p)
    check("准备：内容 = OLD", got == b"OLD", f"实际 {got!r}")

    # 1. CREATE_NEW 撞名
    h, err = open_(p, GENERIC_WRITE, CREATE_NEW)
    if ok(h):
        k32.CloseHandle(h)
        check("CREATE_NEW 撞名应失败", False, "居然成功了（会直接覆盖，资源管理器永远不弹框）")
    else:
        check("CREATE_NEW 撞名返回 ERROR_FILE_EXISTS(80)", err == 80, f"实际 err={err}")
    got, _ = read_all(p)
    check("撞名失败后旧内容没被破坏", got == b"OLD", f"实际 {got!r}")

    # 2. CREATE_ALWAYS 覆盖
    h, err = open_(p, GENERIC_WRITE, CREATE_ALWAYS)
    if not ok(h):
        check("CREATE_ALWAYS 覆盖应成功", False, f"err={err}")
    else:
        write_all(h, b"NEW-CONTENT")
        k32.CloseHandle(h)
        check("CREATE_ALWAYS 覆盖成功", True)
        got, _ = read_all(p)
        check("覆盖后内容 = NEW-CONTENT（无旧尾巴）", got == b"NEW-CONTENT", f"实际 {got!r}")

    # 3. OPEN_ALWAYS 不能截断
    h, err = open_(p, GENERIC_WRITE, OPEN_ALWAYS)
    if not ok(h):
        check("OPEN_ALWAYS 打开已存在文件", False, f"err={err}")
    else:
        k32.CloseHandle(h)
        check("OPEN_ALWAYS 打开成功", True)
        got, _ = read_all(p)
        check("OPEN_ALWAYS 后内容仍是 NEW-CONTENT", got == b"NEW-CONTENT", f"实际 {got!r}")

    # 4. 随机写：改中间几个字节，其余必须保留
    h, err = open_(p, GENERIC_WRITE | GENERIC_READ, OPEN_EXISTING)
    if not ok(h):
        check("OPEN_EXISTING 写打开", False, f"err={err}")
    else:
        k32.SetFilePointer(h, 4, None, 0)  # 0 = FILE_BEGIN
        write_all(h, b"XYZ")
        k32.CloseHandle(h)
        got, _ = read_all(p)
        check("随机写后内容 = NEW-XYZTENT（旧内容保留）", got == b"NEW-XYZTENT", f"实际 {got!r}")

    # 5. 编辑器式：写打开 + SetEndOfFile 截断 + 重写
    h, err = open_(p, GENERIC_WRITE | GENERIC_READ, OPEN_EXISTING)
    if not ok(h):
        check("编辑器式截断重写：打开", False, f"err={err}")
    else:
        k32.SetFilePointer(h, 3, None, 0)
        k32.SetEndOfFile(h)
        k32.SetFilePointer(h, 0, None, 0)
        write_all(h, b"ABC")
        k32.CloseHandle(h)
        got, _ = read_all(p)
        check("截断重写后内容 = ABC", got == b"ABC", f"实际 {got!r}")

    # 6. 追加写：末尾追加不能动前面的内容
    h, err = open_(p, GENERIC_WRITE | GENERIC_READ, OPEN_EXISTING)
    if not ok(h):
        check("追加写：打开", False, f"err={err}")
    else:
        k32.SetFilePointer(h, 0, None, 2)  # 2 = FILE_END
        write_all(h, b"DEF")
        k32.CloseHandle(h)
        got, _ = read_all(p)
        check("追加后内容 = ABCDEF", got == b"ABCDEF", f"实际 {got!r}")

    print()
    print("全部通过" if fails == 0 else f"{fails} 项失败")
    return 0 if fails == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
