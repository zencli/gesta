
import subprocess, os, shutil, re

_PROJECT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ZF = os.environ.get("ZF_BIN", os.path.join(_PROJECT, "target", "release", "gesta"))
N = 500
base = [f"line {i}: some content padding padding padding" for i in range(N)]

def workload(root, use_git):
    shutil.rmtree(root, ignore_errors=True); os.makedirs(root); os.chdir(root)
    if use_git:
        subprocess.run(["git","init","-q"], check=True)
        subprocess.run(["git","config","user.email","a@b"], check=True)
        subprocess.run(["git","config","user.name","a"], check=True)
    else:
        subprocess.run([ZF,"init"], capture_output=True, check=True)
    lines = list(base)
    open("big.txt","w").write("\n".join(lines)+"\n")
    if use_git:
        subprocess.run(["git","add","-A"], check=True); subprocess.run(["git","commit","-qm","v0"], check=True)
    else:
        o = subprocess.run([ZF,"changeset","create","v0","--author","b"], capture_output=True, text=True).stdout
        cs = re.search(r"\b(cs[0-9a-f]{6,})\b", o).group(1)
        subprocess.run([ZF,"changeset","commit",cs], capture_output=True, check=True)
    for r in range(1,31):
        lines[200] = f"line 200: EDITED round {r}"
        open("big.txt","w").write("\n".join(lines)+"\n")
        if use_git:
            subprocess.run(["git","add","-A"], check=True); subprocess.run(["git","commit","-qm",f"e{r}"], check=True)
        else:
            o = subprocess.run([ZF,"changeset","create",f"e{r}","--author","b"], capture_output=True, text=True).stdout
            cs = re.search(r"\b(cs[0-9a-f]{6,})\b", o).group(1)
            subprocess.run([ZF,"changeset","commit",cs], capture_output=True, check=True)
    # Only the repository metadata counts. Summing the working tree would count
    # the same file twice and say nothing about how the history is stored.
    target = ".git" if use_git else ".gesta"
    total = sum(os.path.getsize(os.path.join(dp,f)) for dp,_,fs in os.walk(target) for f in fs)
    return total

zf = workload("/tmp/zf_cmp2", False); os.chdir("/")
gt = workload("/tmp/git_cmp2", True); os.chdir("/")
print(f"gesta: {zf/1024:.0f} KB")
print(f"git     : {gt/1024:.0f} KB")
print(f"ratio   : {zf/gt:.2f}x  (truoc khi nen: 8.9x)")

# 1000 file
def many(root, use_git):
    shutil.rmtree(root, ignore_errors=True); os.makedirs(root); os.chdir(root)
    if use_git:
        subprocess.run(["git","init","-q"], check=True); subprocess.run(["git","config","user.email","a@b"]); subprocess.run(["git","config","user.name","a"])
    else:
        subprocess.run([ZF,"init"], capture_output=True)
    for i in range(1000): open(f"f{i}.txt","w").write(f"content {i}\n")
    if use_git:
        subprocess.run(["git","add","-A"]); subprocess.run(["git","commit","-qm","x"])
        # Packed, because comparing against loose objects measures git's unpacked form: 1000 loose
        # objects cost about 5 MB and say nothing about what a repository costs to keep.
        subprocess.run(["git","gc","-q","--aggressive","--prune=now"], capture_output=True)
        store = ".git"
    else:
        o = subprocess.run([ZF,"changeset","create","x","--author","b"], capture_output=True, text=True).stdout
        cs = [w for w in o.split() if w.startswith("cs")][0]
        subprocess.run([ZF,"changeset","commit",cs], capture_output=True)
        # Vacuumed, so both sides are measured after their own compaction. The earlier version measured
        # this tool mid-WAL and git packed, which compares two different states.
        db = None
        for dp, _, fs in os.walk(".gesta"):
            for f in fs:
                if f.endswith(".db"):
                    db = os.path.join(dp, f)
        if db:
            subprocess.run(["sqlite3", db, "VACUUM;"], capture_output=True)
        store = ".gesta"
    # Only the store: the working files exist on both sides and counting them inflated both numbers.
    return sum(os.path.getsize(os.path.join(dp,f)) for dp,_,fs in os.walk(store) for f in fs)

z2 = many("/tmp/zf_many2", False); os.chdir("/")
g2 = many("/tmp/git_many2", True); os.chdir("/")
print(f"\n1000 file nho: gesta {z2//1024} KB vs git {g2//1024} KB = {z2/g2:.2f}x")
