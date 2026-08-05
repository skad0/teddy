#!/usr/bin/env python3
"""The deliberately small, non-claim-producing S9 phase-one harness."""
from __future__ import annotations
import argparse, codecs, hashlib, json, math, os, pty, select, signal, struct, subprocess, statistics, re
import sys, tempfile, time, unittest, resource, platform, shutil, shlex, uuid
from pathlib import Path

ROOT=Path(__file__).resolve().parent; REPO=ROOT.parent
CORPUS=json.loads((ROOT/"corpora.json").read_text()); FIX=CORPUS["fixtures"]
PHASE="phase1"; RESULT_SCHEMA="teddy-s9-phase1-result-5"; MANIFEST_SCHEMA="teddy-s9-phase1-manifest-3"
PHASE2_SCHEMA="teddy-s9-phase2-result-2"
PHASE2_METHODOLOGY="s9-c1-post-readiness-observer-1"
PHASE2_HISTORICAL_CONTEXT="The prior canonical Phase 2 C1 result was FAIL and used pre-drain identity/PGID observation; it remains immutable historical context and is not silently overwritten."
PHASE2_ENV_ALLOWLIST={"LANG","LC_ALL","LC_CTYPE","TERM","TZ","XDG_CONFIG_HOME","XDG_DATA_HOME","XDG_STATE_HOME","XDG_CACHE_HOME","TMPDIR"}
CLAIMS={c:{"status":"NOT_MEASURED","reason":"Phase 1 measures harness health only"} for c in "C1 C2 C3 C4 C5".split()}

def validate_config(config=CORPUS):
    required={"kind","size","sha256","markers","post_edit_sha256"}
    for name,spec in config["fixtures"].items():
        missing=required-set(spec)
        if missing: raise ValueError(f"{name}: missing declarative keys: {','.join(sorted(missing))}")
        if not isinstance(spec["markers"],list) or not all(isinstance(x,str) and x for x in spec["markers"]): raise ValueError(f"{name}: invalid literal markers")
        if spec["kind"]=="source" and not all(k in spec for k in ("row_template","page_rows","page_endpoint")): raise ValueError(f"{name}: incomplete page declaration")
        if spec["kind"]=="binary" and not all(k in spec for k in ("ascii_head","ascii_tail","status_marker","escaped_expectations")): raise ValueError(f"{name}: incomplete binary declaration")
    return True

def sha(path):
    h=hashlib.sha256(); n=0
    with open(path,"rb") as f:
        while True:
            b=f.read(65536)
            if not b: break
            h.update(b); n+=len(b)
    return h.hexdigest(),n

def fixture_bytes(name):
    spec=FIX[name]
    if spec["kind"]=="source":
        # Every record, including its LF, is exactly 64 bytes.
        return b"".join((spec["row_template"].format(i=i, value=i%997)[:63].ljust(63)+"\n").encode() for i in range(spec["lines"]))
    if spec["kind"]=="crlf": return b"S9_CRLF_HEAD\r\nsecond fixed row\r\nS9_CRLF_TAIL"
    if spec["kind"]=="invalid": return b"S9_INVALID_HEAD\xff\xfeinvalid-row"
    if spec["kind"]=="binary": return b"DATA_HEAD\0"+bytes(range(1,256))*31+b"DATA_TAIL"+b"\xff"*(8192-10-7905-9)
    if spec["kind"]=="reproducer": return b"RENDER_HEAD\xc3\xa9\xc3"
    raise KeyError(name)

def _properties(name,data):
    try: data.decode("utf8"); valid=True
    except UnicodeDecodeError: valid=False
    return {"sha256":hashlib.sha256(data).hexdigest(),"size":len(data),"newline":data.count(b"\n"),"has_nul":b"\0" in data,"valid_utf8":valid}

def generate(out):
    validate_config()
    out=Path(out); out.mkdir(parents=True,exist_ok=True); manifests=[]
    required={"kind","size","sha256","markers","post_edit_sha256"}
    for name,spec in FIX.items():
        missing=required-set(spec)
        if missing: raise ValueError(f"{name}: missing declarative keys: {','.join(sorted(missing))}")
        data=fixture_bytes(name); p=out/name; p.write_bytes(data); digest,size=sha(p)
        if size!=spec["size"] or digest!=spec["sha256"]: raise RuntimeError(f"{name}: generated fixture disagrees with declaration")
        if hashlib.sha256(b"X"+data).hexdigest()!=spec["post_edit_sha256"]: raise RuntimeError(f"{name}: post-edit digest mismatch")
        if not isinstance(spec["markers"],list) or not spec["markers"]: raise ValueError(f"{name}: markers must be non-empty")
        props=dict(spec); props.pop("markers",None); props.pop("post_edit_sha256",None); props["actual"]=_properties(name,data)
        m={"schema":MANIFEST_SCHEMA,"name":name,"file":name,"size":size,"sha256":digest,"properties":props,
           "screen_markers":spec["markers"],"post_edit_sha256":spec["post_edit_sha256"]}
        (out/(name+".manifest.json")).write_text(json.dumps(m,indent=2,sort_keys=True)+"\n"); manifests.append(m)
    aggregate={"schema":MANIFEST_SCHEMA,"corpus_schema":CORPUS["schema"],"seed":CORPUS["seed"],"fixtures":manifests}
    raw=json.dumps(aggregate,sort_keys=True,separators=(",",":"),ensure_ascii=False).encode()
    aggregate["aggregate_sha256"]=hashlib.sha256(raw).hexdigest()
    (out/"manifest.json").write_text(json.dumps(aggregate,indent=2,sort_keys=True)+"\n")
    return manifests

def _winsize(fd,g):
    import fcntl,termios
    fcntl.ioctl(fd,termios.TIOCSWINSZ,struct.pack("HHHH",g[1],g[0],0,0))

def isolated_env(home):
    return {"HOME":str(home),"XDG_CONFIG_HOME":str(home/"config"),"XDG_DATA_HOME":str(home/"data"),"XDG_STATE_HOME":str(home/"state"),"XDG_CACHE_HOME":str(home/"cache"),"TMPDIR":str(home/"tmp"),"TERM":"xterm-256color","LANG":"C.UTF-8","LC_ALL":"C.UTF-8","PATH":os.environ.get("PATH","/usr/bin:/bin")}

class Screen:
    def __init__(self,cols=200,rows=50):
        self.cols,self.rows=cols,rows; self.x=self.y=0; self.alt=False; self.cursor_visible=True; self.unsupported=[]; self._pending=b""; self._decoder=codecs.getincrementaldecoder("utf-8")("strict")
        self.main=[[' ']*cols for _ in range(rows)]; self.alternate=[[' ']*cols for _ in range(rows)]; self.grid=self.main; self.saved_main=(0,0); self.saved_alt=(0,0)
    def _put(self,ch):
        if ch in "\a": return
        if ch=="\n": self.y=min(self.rows-1,self.y+1); return
        if ch=="\r": self.x=0; return
        if ch=="\b": self.x=max(0,self.x-1); return
        if ch=="\t": self.x=min(self.cols-1,self.x+8-self.x%8); return
        if ord(ch)>=32:
            self.grid[self.y][min(self.x,self.cols-1)]=ch; self.x=min(self.cols-1,self.x+1)
    def feed(self,data):
        self._pending+=data; i=0
        while i<len(self._pending):
            b=self._pending[i]
            if b==27:
                if i+1>=len(self._pending): break
                if self._pending[i+1]==91:
                    j=i+2
                    while j<len(self._pending) and not 64<=self._pending[j]<=126:j+=1
                    if j==len(self._pending): break
                    self.csi(self._pending[i+2:j].decode("ascii","replace"),chr(self._pending[j])); i=j+1; continue
                if self._pending[i+1] in (55,56):
                    if self._pending[i+1]==55: self.saved_alt=(self.x,self.y)
                    else: self.x,self.y=self.saved_alt
                    i+=2; continue
                self.unsupported.append(self._pending[i:i+2].hex()); i+=2; continue
            if b<32: self._put(chr(b)); i+=1; continue
            try:
                need=1 if b<128 else (2 if b<224 else (3 if b<240 else 4))
                if len(self._pending)-i<need: break
                ch=self._pending[i:i+need].decode("utf8")
                self._put(ch); i+=need
            except (UnicodeDecodeError,IndexError):
                if len(self._pending)-i<4 and b>=128: break
                self.unsupported.append(f"invalid-utf8:{b:02x}"); i+=1
        self._pending=self._pending[i:]
    def csi(self,p,f):
        private=p.startswith("?"); q=p[1:] if private else p
        if f in "hl" and private and q in ("25","47","1047","1049"):
            if q=="25": self.cursor_visible=f=="h"; return
            if f=="h":
                if q=="1049": self.saved_main=(self.x,self.y)
                self.alt=True; self.grid=self.alternate; self.x=self.y=0
                if q in ("1047","1049"): self.alternate=[[' ']*self.cols for _ in range(self.rows)]; self.grid=self.alternate
            else:
                self.alt=False; self.grid=self.main
                if q=="1049": self.x,self.y=self.saved_main
            return
        try: a=[int(x or 1) for x in p.split(';')] if p else [1]
        except ValueError: self.unsupported.append(p+f); return
        n=a[0]
        if f in "ABCD": self.y=max(0,min(self.rows-1,self.y+({'A':-n,'B':n}.get(f,0)))); self.x=max(0,min(self.cols-1,self.x+({'C':n,'D':-n}.get(f,0))))
        elif f in "Hf": self.y=max(0,min(self.rows-1,a[0]-1)); self.x=max(0,min(self.cols-1,(a[1] if len(a)>1 else 1)-1))
        elif f=="J" and n in (2,3): self.grid[:]=[[' ']*self.cols for _ in self.grid]
        elif f=="K":
            mode=0 if not p else n
            if mode==2: self.grid[self.y]=[' ']*self.cols
            elif mode==1: self.grid[self.y]=[' ']*min(self.cols,self.x+1)+self.grid[self.y][self.x+1:]
            elif mode==0: self.grid[self.y]=self.grid[self.y][:self.x]+[' ']*(self.cols-self.x)
            else: self.unsupported.append(p+f)
        elif f in ("m","s","u"): pass
        else: self.unsupported.append(p+f)
    def text(self): return [''.join(r).rstrip() for r in self.grid]
    def contains(self,m): return any(m in r for r in self.text())
    def finish(self):
        """Finalize stream state; truncated terminal input is unsupported."""
        if self._pending:
            self.unsupported.append("incomplete-terminal-input:"+self._pending.hex())
            self._pending=b""

class Session:
    def __init__(self,argv,geometry=(200,50),timeout=4,output_cap=4<<20,stderr_cap=1<<20):
        self.argv=argv; self.geometry=geometry; self.timeout=timeout; self.output_cap=output_cap; self.stderr_cap=stderr_cap; self.out=bytearray(); self.err=bytearray(); self.trace=bytearray(); self.trace_bytes=0; self.trace_discarded_bytes=0; self.trace_hash=hashlib.sha256(); self.stderr_full_bytes=self.output_full_bytes=0; self.trace_cap=4<<20; self.screen=Screen(*geometry); self.actions=[]; self.endpoint_snapshots=[]; self.output_capped=self.stderr_capped=self.timed_out=False; self.status=None; self.pid=-1; self.root_pid=-1; self.pgid=-1; self.original_pgid=-1; self.closed=False; self.cleanup_error=None; self.drain_reason=None; self.last_output=None; self.first_output=None; self.output_events=[]; self.spawn_at=None; self.descendants=[]; self.pgid_before=[]; self.pgid_after=[]; self.pgid_probe_error=None; self.pgid_before_probe_error=None; self.pgid_after_probe_error=None; self.group_probe_started_ms=None; self.group_probe_finished_ms=None; self.group_probe_ms=None; self.drain_complete=False; self.drain_deadline=False; self.pty_eof=self.stderr_eof=False
        self.extra_env={}
    def spawn(self,home,cwd,preflight=True):
        self.fork_started_wall=time.monotonic(); self.master,self.slave=pty.openpty(); self.er,self.ew=os.pipe(); self.pid=os.fork()
        if self.pid==0:
            try:
                os.setsid(); import fcntl,termios; fcntl.ioctl(self.slave,termios.TIOCSCTTY,0); _winsize(self.slave,self.geometry); os.dup2(self.slave,0); os.dup2(self.slave,1); os.dup2(self.ew,2); os.chdir(cwd); env=isolated_env(Path(home)); env.update(self.extra_env); os.execve(self.argv[0],self.argv,env)
            except BaseException: os.write(2,b"S9_EXEC_FAILURE\n"); os._exit(127)
        os.close(self.slave); os.close(self.ew); self.parent_start_wall=time.monotonic(); _winsize(self.master,self.geometry); os.set_blocking(self.master,False); os.set_blocking(self.er,False); self.t0=time.monotonic(); self.spawn_at=0.0; self.root_pid=self.pid; self.pgid=self.pid; self.original_pgid=self.pid
        if preflight:
            probe_started=time.monotonic(); self.identity_probe_started_ms=(probe_started-self.t0)*1000; self.identity=self._ps(); self.identity_probe_finished_ms=(time.monotonic()-self.t0)*1000; self.identity_probe_ms=self.identity_probe_finished_ms-self.identity_probe_started_ms
        else:
            self.identity={"verified":False,"deferred":True}; self.identity_probe_started_ms=None; self.identity_probe_finished_ms=None; self.identity_probe_ms=0.0
    def _ps(self):
        try:
            row=subprocess.check_output(["ps","-p",str(self.pid),"-o","pid=,ppid=,comm=,args="],text=True,stderr=subprocess.DEVNULL).strip(); tokens=shlex.split(row.split(None,3)[3]) if row and len(row.split(None,3))==4 else []
            return {"observed":row,"verified":tokens==self.argv,"argv":tokens}
        except Exception as e:return {"verified":False,"error":str(e)}
    def poll(self):
        if self.pid>0:
            got,st=os.waitpid(self.pid,os.WNOHANG)
            if got:self.status=st; self.pid=-1
    def _read(self,wait=.02):
        ready,_,_=select.select([self.master,self.er],[],[],wait)
        now=time.monotonic()-self.t0
        for fd in ready:
            try:b=os.read(fd,65536)
            except OSError as e:
                if e.errno==5:
                    if fd==self.master:self.pty_eof=True; self.screen.finish()
                    else:self.stderr_eof=True
                    b=b""
                else: raise
            if not b:
                if fd==self.master: self.pty_eof=True; self.screen.finish()
                else: self.stderr_eof=True
                continue
            if fd==self.master:
                self.trace_bytes+=len(b); self.output_full_bytes+=len(b); self.trace_hash.update(b); kept=max(0,self.trace_cap-len(self.trace)); self.trace.extend(b[:kept]); self.trace_discarded_bytes+=max(0,len(b)-kept); self.screen.feed(b)
                if len(self.out)+len(b)>self.output_cap: self.output_capped=True
                self.out.extend(b[:max(0,self.output_cap-len(self.out))]); self.last_output=now; self.output_events.append(now)
                if self.first_output is None: self.first_output=now
            else:
                self.stderr_full_bytes+=len(b)
                if len(self.err)+len(b)>self.stderr_cap: self.stderr_capped=True
                self.err.extend(b[:max(0,self.stderr_cap-len(self.err))])
    def until(self,predicate,name="endpoint",after_output_count=None):
        """Wait for a predicate, optionally requiring a new PTY output event.

        The timestamp belongs to the PTY read which changed the screen, not to
        the later predicate evaluation.
        """
        baseline=len(self.output_events) if after_output_count is None else after_output_count
        end=time.monotonic()+self.timeout
        while time.monotonic()<end:
            self._read(); self.poll()
            if len(self.output_events)>baseline and predicate(self.screen):
                event_index=len(self.output_events)-1
                return {"name":name,"matched":True,"matched_at":self.output_events[event_index],"event_index":event_index,"snapshot":list(self.screen.text())}
            if self.status is not None: return {"name":name,"matched":False,"matched_at":None,"snapshot":None}
        self.timed_out=True; return {"name":name,"matched":False,"matched_at":None,"snapshot":None}
    def ready(self,filename,head):
        r=self.until(lambda sc: sc.alt and sc.contains(filename) and sc.contains(head),"readiness"); self.readiness=r
        return bool(r.get("matched")) and self.quiet()
    def quiet(self):
        started=time.monotonic(); last_byte=started; self.t_quiet=getattr(self,"quiet_interval",.005); deadline=last_byte+self.t_quiet; limit=started+max(.5,self.timeout)
        while time.monotonic()<deadline and time.monotonic()<limit:
            before=len(self.output_events); self._read(max(.001,deadline-time.monotonic())); self.poll()
            if len(self.output_events)!=before: last_byte=self.output_events[-1]; deadline=time.monotonic()+self.t_quiet
        now=time.monotonic(); complete=now>=deadline; self.quiet_at=now-self.t0; self.quiet_complete=self.quiet_at if complete else None; self.last_quiet_gap=self.quiet_at-(self.last_output or 0); return self.status is None and complete
    def write(self,data,expected,name="action"):
        if not self.quiet(): return False
        os.write(self.master,data); write=time.monotonic()-self.t0; event_start=len(self.output_events); endpoint=self.until(expected,name,event_start); quiet=self.quiet(); after=self.output_events[event_start:]; first=after[0] if after else None; last=after[-1] if after else None; matched=bool(endpoint.get("matched")); rows=endpoint.get("snapshot") if matched else self.screen.text()
        rows=rows if isinstance(rows,list) else self.screen.text(); snap={"name":endpoint["name"],"at":endpoint.get("matched_at"),"kind":"action","rows":rows,"sha256":hashlib.sha256("\n".join(rows).encode()).hexdigest()}; self.endpoint_snapshots.append(snap)
        self.actions.append({"name":snap["name"],"write":write,"output_event_sequence_at_write":event_start,"first_output_after_write":first,"endpoint":endpoint.get("matched_at"),"endpoint_event_index":endpoint.get("event_index"),"last_output":last,"quiet_complete":self.quiet_at,"quiet_gap":self.last_quiet_gap,"output_event_count":len(after),"endpoint_found":matched,"bytes":data.hex()}); return matched
    def close(self):
        if self.closed:return True
        clean=False
        try:
            # Cleanup is not a measured action and must not create/replace an endpoint.
            try: os.write(self.master,b"\x11")
            except OSError as e:
                if e.errno != 5: raise
            self.cleanup_write=time.monotonic()-self.t0
            if not self.pgid_before:
                ok,self.pgid_before,self.pgid_before_probe_error=process_group(self.original_pgid)
            deadline=time.monotonic()+max(.5,self.timeout)
            while self.pid>0 and time.monotonic()<deadline:self._read(.01); self.poll()
            clean=self.status is not None and os.WIFEXITED(self.status) and os.WEXITSTATUS(self.status)==0
            if self.pgid>0:
                try: os.killpg(self.pgid,signal.SIGKILL)
                except OSError as e:
                    if self.status is None and e.errno not in (3,5): self.cleanup_error=str(e)
                try: os.kill(self.root_pid,signal.SIGKILL)
                except OSError: pass
                deadline=time.monotonic()+.5
                while self.pid>0 and time.monotonic()<deadline: self.poll(); time.sleep(.005)
                if self.pid>0: self.cleanup_error=self.cleanup_error or "bounded reap deadline exceeded"
            drain_deadline=time.monotonic()+.5
            while time.monotonic()<drain_deadline and not (self.pty_eof and self.stderr_eof):
                try: self._read(0)
                except OSError as e:
                    if e.errno != 5: self.cleanup_error=self.cleanup_error or str(e)
                self.poll()
            self.drain_complete=self.pty_eof and self.stderr_eof
            self.drain_deadline=not self.drain_complete
            self.drain_reason="both channels reached EOF/EIO" if self.drain_complete else "drain deadline expired before both channels reached EOF/EIO"
            ok,self.pgid_after,self.pgid_after_probe_error=process_group(self.original_pgid)
            if not ok: self.pgid_probe_error=self.pgid_after_probe_error
            self.descendants=self.pgid_after
        except Exception as e:self.cleanup_error=str(e); return False
        finally:
            for fd in (getattr(self,"master",-1),getattr(self,"er",-1)):
                try:os.close(fd)
                except OSError:pass
            self.closed=True
        return clean
    def record(self):
        st=self.status; exited=st is not None and os.WIFEXITED(st)
        exit_code=os.WEXITSTATUS(st) if st is not None and exited else None
        sig=os.WTERMSIG(st) if st is not None and not exited else None
        return {"spawn":self.spawn_at,"readiness":getattr(self,"readiness",None),"exit":exit_code,"signal":sig,"timed_out":self.timed_out,"pty_output_capped":self.output_capped,"stderr_capped":self.stderr_capped,"trace_bytes":self.trace_bytes,"trace_retained_bytes":len(self.trace),"trace_discarded_bytes":self.trace_discarded_bytes,"trace_sha256":self.trace_hash.hexdigest(),"output_full_bytes":self.output_full_bytes,"output_retained_bytes":len(self.out),"stderr_full_bytes":self.stderr_full_bytes,"stderr_retained_bytes":len(self.err),"actions":self.actions,"endpoint_snapshots":self.endpoint_snapshots,"screen":self.screen.text(),"unsupported":self.screen.unsupported,"stderr":self.err.decode("utf8","replace"),"exec_failed":b"S9_EXEC_FAILURE" in self.err,"identity":getattr(self,"identity",{"verified":False}),"final_drain":self.closed,"drain_complete":self.drain_complete,"drain_deadline":self.drain_deadline,"drain_reason":self.drain_reason,"pty_eof":self.pty_eof,"stderr_eof":self.stderr_eof,"cleanup_error":self.cleanup_error,"descendants_left":bool(self.descendants) or self.pid>0,"descendants":self.descendants,"pgid_before":self.pgid_before,"pgid_after":self.pgid_after,"pgid_probe_error":self.pgid_probe_error,"pgid_before_probe_error":self.pgid_before_probe_error,"pgid_after_probe_error":self.pgid_after_probe_error,"reaped":self.status is not None,"pgid":self.original_pgid}

def stage(root):
    release=REPO/"target"/"release"; binaries={"teddy":release/"teddy","teddy-highlight":release/"teddy-highlight"}
    if not all(p.is_file() for p in binaries.values()): raise RuntimeError("release teddy and teddy-highlight are required")
    root=Path(root); result={}
    for profile,names in (("bare",("teddy",)),("shipped",("teddy","teddy-highlight"))):
        d=root/profile; d.mkdir(); files={}
        for n in names:
            dst=d/n; shutil.copy2(binaries[n],dst); h,z=sha(dst)
            try: version=subprocess.check_output([str(dst),"--version"],text=True,stderr=subprocess.STDOUT,timeout=2).strip()
            except Exception: version="UNAVAILABLE"
            files[n]={"sha256":h,"size":z,"version":version,"identity_verified":dst.is_file() and h==sha(binaries[n])[0]}
        result[profile]={"exact_files":sorted(p.name for p in d.iterdir()),"files":files,"identity_source":{n:{"sha256":sha(binaries[n])[0],"size":sha(binaries[n])[1],"arch":platform.machine()} for n in names},"helper_probe":files.get("teddy-highlight")}
    return result

def process_tree(pid):
    """Return a bounded ps snapshot, including descendants of the session."""
    try:
        rows=subprocess.check_output(["ps","-axo","pid=,ppid=,comm=,args="],text=True,stderr=subprocess.DEVNULL).splitlines()
        parsed=[]
        for row in rows:
            bits=row.strip().split(None,3)
            if len(bits)==4:
                try: parsed.append({"pid":int(bits[0]),"ppid":int(bits[1]),"comm":bits[2],"args":bits[3]})
                except ValueError: pass
        ids={pid}; changed=True
        while changed:
            changed=False
            for r in parsed:
                if r["ppid"] in ids and r["pid"] not in ids: ids.add(r["pid"]); changed=True
        return [r for r in parsed if r["pid"] in ids]
    except Exception:
        return []

def process_group(pgid):
    """Return the independently verifiable members of an original PGID."""
    if pgid <= 0: return False, [], "invalid PGID"
    try:
        rows=subprocess.check_output(["ps","-axo","pid=,ppid=,pgid=,comm=,command="],text=True,stderr=subprocess.DEVNULL).splitlines()
        out=[]
        for row in rows:
            b=row.strip().split(None,4)
            if len(b)>=4:
                try:
                    if int(b[2])==pgid: out.append({"pid":int(b[0]),"ppid":int(b[1]),"pgid":int(b[2]),"comm":b[3],"command":b[4] if len(b)>4 else ""})
                except ValueError: pass
        return True, out, None
    except Exception as e: return False, [], str(e)

def exact_group_identity(probe, root_argv, helper_argv=None):
    ok,members,error=probe
    if not ok: return False, error or "process-group probe failed"
    expected=[root_argv]+(([helper_argv]) if helper_argv else [])
    actual=[]
    for member in members:
        try: actual.append(shlex.split(member["command"]))
        except ValueError: return False, "malformed process-group argv"
    if sorted(actual)!=sorted(expected): return False, "unexpected process-group identity"
    return True, None

def valid_action_timing(action):
    """Validate the complete causal timing chain for one measured action."""
    vals=(action.get("write"),action.get("first_output_after_write"),action.get("endpoint"),action.get("last_output"),action.get("quiet_complete"))
    return (action.get("endpoint_found") is True and all(v is not None for v in vals)
            and vals[0] < vals[1] <= vals[2] <= vals[3] <= vals[4]
            and action.get("quiet_gap",0) >= .005)

def valid_binary_evidence(rows,spec):
    status=rows[49] if len(rows)>49 else ""
    return (spec["ascii_head"] in "\n".join(rows)
            and all(token in status for token in spec["status_tokens"])
            and all(any(marker in row for row in rows) for marker in spec["escaped_expectations"]))

def scenario(profile,root,corpus,out,name,timeout,mode=None):
    source=Path(corpus)/name; copy=Path(corpus)/("private-"+profile+"-"+name); shutil.copyfile(source,copy); source_digest=sha(source)[0]
    sn={"code-1m.rs":"page-down" if mode else "open-screen","edit-crlf.txt":"insert-save-crlf","edit-invalid.txt":"insert-save-invalid","binary-safe.bin":"read-only-binary","binary-render-repro.bin":"renderer-diagnostic"}[name]
    head=FIX[name]["markers"][0]; marker=copy.name; s=Session([str(Path(root)/profile/"teddy"),str(copy)],timeout=timeout); home=Path(tempfile.mkdtemp(prefix="s9-home-")); [(home/x).mkdir() for x in ("config","data","state","cache","tmp")]; reasons=[]; status="PASS"; live_tree=[]; helper_seen=False
    root_argv=[str(Path(root)/profile/"teddy"),str(copy)]; helper_argv=[str(Path(root)/profile/"teddy-highlight")]; identity_ok=False; identity_error="identity probe did not run"
    try:
        s.spawn(home,home)
        probe=process_group(s.original_pgid); ok,live_tree,probe_error=probe
        s.pgid_before=list(live_tree)
        if not ok: status="INCONCLUSIVE"; reasons.append("process-group probe failed: "+str(probe_error))
        identity_ok,identity_error=exact_group_identity(probe,root_argv,helper_argv if profile=="shipped" else None)
        helper_seen=identity_ok if profile=="shipped" else False
        if profile=="bare" and not identity_ok: status="INCONCLUSIVE"; reasons.append(identity_error)
        if profile=="shipped" and not identity_ok: status="INCONCLUSIVE"; reasons.append(identity_error)
        if sn=="renderer-diagnostic": s.until(lambda sc: sc.contains("RENDER_HEAD"))
        elif not s.ready(marker,head): status="INCONCLUSIVE"; reasons.append("missing readiness endpoint")
        else:
            rows=s.screen.text(); s.endpoint_snapshots.append({"name":"readiness","at":time.monotonic()-s.t0,"kind":"readiness","rows":rows,"sha256":hashlib.sha256("\n".join(rows).encode()).hexdigest()})
            probe=process_group(s.original_pgid); ok,live_tree,probe_error=probe
            if not ok: status="INCONCLUSIVE"; reasons.append("process-group probe failed: "+str(probe_error))
            identity_ok,identity_error=exact_group_identity(probe,root_argv,helper_argv if profile=="shipped" else None)
            helper_seen=identity_ok if profile=="shipped" else False
            if not identity_ok: status="INCONCLUSIVE"; reasons.append(identity_error)
        if sn=="page-down" and status=="PASS":
            rows=FIX[name]["page_rows"]; endpoint=FIX[name]["page_endpoint"]; markers=FIX[name]["markers"]
            if not s.write(b"\033[6~",lambda sc:sc.contains(markers[1]) and sc.contains(markers[2]),"page_down"): status="INCONCLUSIVE"; reasons.append("page-down endpoint missing: "+endpoint)
        elif sn.startswith("insert-save") and status=="PASS":
            if not s.write(b"X",lambda sc:sc.contains("X"),"insert"): status="INCONCLUSIVE"; reasons.append("insert endpoint missing")
            elif not s.write(b"\x13",lambda sc:sc.contains("saved"),"save"): status="INCONCLUSIVE"; reasons.append("save status endpoint missing")
            if hashlib.sha256(copy.read_bytes()).hexdigest()!=FIX[name]["post_edit_sha256"]: status="FAIL"; reasons.append("post-save digest mismatch")
            if sha(source)[0]!=source_digest: status="FAIL"; reasons.append("source changed")
        elif sn=="read-only-binary" and status=="PASS":
            content_evidence=(FIX[name]["ascii_head"],)+tuple(FIX[name]["escaped_expectations"])
            missing=[x for x in content_evidence if not s.screen.contains(x)]
            status_row=s.screen.text()[49] if len(s.screen.text())>49 else ""
            if not ("RO" in status_row and "BIN" in status_row): missing.append("RO/BIN status row 49")
            if missing: status="INCONCLUSIVE"; reasons.append("missing binary evidence: "+','.join(missing))
        if not s.close() and sn!="renderer-diagnostic": status="FAIL"; reasons.append("clean close failed")
    except Exception as e: status="INCONCLUSIVE"; reasons.append(type(e).__name__+": "+str(e)); s.close()
    if s.timed_out: status="INCONCLUSIVE"; reasons.append("timeout")
    if s.output_capped: status="INCONCLUSIVE"; reasons.append("pty output cap")
    if s.stderr_capped: status="INCONCLUSIVE"; reasons.append("stderr cap")
    if s.screen.unsupported: status="INCONCLUSIVE"; reasons.append("unsupported terminal traffic")
    for action in s.actions:
        if action.get("name")!="cleanup":
            if not valid_action_timing(action):
                status="INCONCLUSIVE"; reasons.append("action timestamp ordering/quiet interval invalid")
    members=[r for r in live_tree if r.get("pid")!=s.root_pid]
    if s.pgid_before_probe_error: status="INCONCLUSIVE"; reasons.append("process-group probe failed")
    if s.cleanup_error: status="INCONCLUSIVE"; reasons.append("cleanup error")
    if not s.closed or not s.record()["reaped"] or s.record()["descendants_left"] or s.pgid_after: status="INCONCLUSIVE"; reasons.append("cleanup/reap/original-PGID verification incomplete")
    if not s.drain_complete: status="INCONCLUSIVE"; reasons.append(s.drain_reason or "drain incomplete")
    if sn!="renderer-diagnostic" and s.status is not None and (not os.WIFEXITED(s.status) or os.WEXITSTATUS(s.status)!=0): status="FAIL"; reasons.append("abnormal/nonzero exit")
    if sn=="renderer-diagnostic":
        observed=(s.status is not None and os.WIFEXITED(s.status) and os.WEXITSTATUS(s.status)==101 and "src/render.rs:229:57" in s.err.decode("utf8","replace") and identity_ok and not s.timed_out and not s.output_capped and not s.stderr_capped and not s.screen.unsupported and not s.cleanup_error and s.pgid_before_probe_error is None and s.pgid_after_probe_error is None and s.pgid_after==[] and not s.descendants and s.pid<0 and s.pty_eof and s.stderr_eof and s.drain_complete and not s.drain_deadline and s.status is not None and not (not os.WIFEXITED(s.status)))
        status="OBSERVED_FAILURE" if observed else "INCONCLUSIVE"
    action_snaps=[x for x in s.endpoint_snapshots if x.get("kind")=="action" and x.get("name")!="cleanup"]
    measured_snaps=action_snaps or [x for x in s.endpoint_snapshots if x.get("kind")=="readiness"]
    endpoint_rows=measured_snaps[-1]["rows"] if measured_snaps else []
    page_rows=FIX[name].get("page_rows",[1,48])
    if sn=="page-down" and (len(endpoint_rows)<=page_rows[1] or not endpoint_rows[page_rows[0]].startswith(FIX[name]["markers"][1]) or not endpoint_rows[page_rows[1]].startswith(FIX[name]["markers"][2])):
        status="INCONCLUSIVE"; reasons.append("page endpoint artifact rows are not exact")
    if sn in ("insert-save-crlf","insert-save-invalid") and (len(endpoint_rows)<2 or not endpoint_rows[1].startswith("X"+head)):
        status="INCONCLUSIVE"; reasons.append("edit endpoint artifact row is not exact")
    if sn=="read-only-binary" and not valid_binary_evidence(endpoint_rows,FIX[name]):
        status="INCONCLUSIVE"; reasons.append("binary endpoint artifact lacks RO/BIN escaped-byte evidence")
    rec={"scenario":sn,"profile":profile,"status":status,"application_diagnostic_status":status if sn=="renderer-diagnostic" else None,"reasons":reasons,"process":s.record(),"profile_identity":{"helper_observed":helper_seen,"identity_confidence":"verified" if identity_ok else "INCONCLUSIVE","live_process_group":live_tree,"required_helper":profile=="shipped","root_argv":root_argv,"helper_argv":helper_argv if profile=="shipped" else None},"endpoint":{"required":head if sn!="page-down" else FIX[name]["page_endpoint"],"observed":bool(measured_snaps),"snapshot":measured_snaps[-1] if measured_snaps else None},"corpus":{"name":name,"sha256":source_digest,"size":source.stat().st_size,"post_edit_sha256":hashlib.sha256(copy.read_bytes()).hexdigest()},"evidence":{}}
    Path(out).mkdir(parents=True,exist_ok=True); trace=Path(out)/(profile+"-"+sn+".trace"); trace.write_bytes(s.trace); err=Path(out)/(profile+"-"+sn+".stderr"); err.write_bytes(s.err); screen=Path(out)/(profile+"-"+sn+"-endpoint.json"); screen.write_text(json.dumps(rec["endpoint"],sort_keys=True));
    def artifact_name(p):
        try: return str(p.relative_to(REPO))
        except ValueError: return str(p.resolve())
    saved=Path(out)/(profile+"-"+sn+"-saved.bin"); shutil.copyfile(copy,saved)
    rec["evidence"]={"trace":{"path":artifact_name(trace),"sha256":sha(trace)[0],"size":trace.stat().st_size,"full_bytes":s.trace_bytes,"retained_bytes":len(s.trace),"discarded_bytes":s.trace_discarded_bytes,"full_sha256":s.trace_hash.hexdigest()},"stderr":{"path":artifact_name(err),"sha256":sha(err)[0],"size":err.stat().st_size},"endpoint_screen":{"path":artifact_name(screen),"sha256":sha(screen)[0],"size":screen.stat().st_size},"saved_output":{"path":artifact_name(saved),"sha256":sha(saved)[0],"size":saved.stat().st_size}}
    shutil.rmtree(home,ignore_errors=True); copy.unlink(missing_ok=True); return rec

def metadata(argv):
    try: commit=subprocess.check_output(["git","rev-parse","HEAD"],cwd=REPO,text=True).strip(); dirty=bool(subprocess.check_output(["git","status","--porcelain"],cwd=REPO))
    except Exception: commit=dirty=None
    probe=lambda cmd: subprocess.check_output(cmd,cwd=REPO,text=True,stderr=subprocess.STDOUT,timeout=2).strip()
    try: rust=probe(["rustc","--version"]); cargo=probe(["cargo","--version"])
    except Exception: rust=cargo=None
    try: filesystem=probe(["stat","-f","%T",str(REPO)])
    except Exception: filesystem=str(REPO.stat().st_dev)
    try: load=os.getloadavg()
    except Exception: load=None
    return {"phase":PHASE,"utc":time.strftime("%Y-%m-%dT%H:%M:%SZ",time.gmtime()),"git":{"commit":commit,"dirty":dirty},"host":{"kernel":platform.release(),"arch":platform.machine(),"cores":os.cpu_count(),"ram_bytes":None,"filesystem":filesystem,"free_bytes":shutil.disk_usage(REPO).free,"load":load,"power":"unavailable"},"python":sys.version.split()[0],"rust":rust,"cargo":cargo,"environment":{"TERM":"xterm-256color","LANG":"C.UTF-8","LC_ALL":"C.UTF-8","HOME":"<isolated>","XDG_CONFIG_HOME":"<isolated>","XDG_DATA_HOME":"<isolated>","XDG_STATE_HOME":"<isolated>","XDG_CACHE_HOME":"<isolated>","TMPDIR":"<isolated>","PATH":"<sanitized>"},"locale":"C.UTF-8","term":"xterm-256color","geometry":[200,50],"limits":{"timeout_seconds":4,"pty_bytes":4<<20,"stderr_bytes":1<<20,"trace_retained_bytes":4<<20,"quiet_ms":5},"geometry_rows":50,"geometry_cols":200,"quiescence_ms":5,"argv":argv,"pty_limitation":"PTY records application emission/transport; they do not claim physical rendering or performance."}

def smoke(a):
    corpus=Path(a.corpus); manifests=generate(corpus); artifact_dir=Path(getattr(a,"artifact_root","") or (ROOT/"artifacts"/(time.strftime("%Y%m%dT%H%M%SZ",time.gmtime())+"-"+uuid.uuid4().hex[:10]))); artifact_dir.mkdir(parents=True,exist_ok=True)
    try: artifact_label=str(artifact_dir.relative_to(REPO))
    except ValueError: artifact_label=str(artifact_dir.resolve())
    result={"schema":RESULT_SCHEMA,"phase":PHASE,"harness_status":"INCONCLUSIVE","application_status":"NOT_MEASURED","claims":CLAIMS,"metadata":metadata(sys.argv[1:]),"artifact_root":{"path":artifact_label,"resolvable_from":"repository root"},"corpus_config_sha256":sha(ROOT/"corpora.json")[0],"corpus_manifest_sha256":sha(corpus/"manifest.json")[0],"profiles":{},"limitations":["No physical rendering or application performance claim","renderer diagnostic separately observed"]}
    with tempfile.TemporaryDirectory(prefix="s9-stage-") as d:
        result["profiles_meta"]=stage(d)
        for p in ("bare","shipped"):
            result["profiles"][p]=[scenario(p,d,corpus,artifact_dir,"edit-crlf.txt",a.timeout),scenario(p,d,corpus,artifact_dir,"code-1m.rs",a.timeout,"page-down"),scenario(p,d,corpus,artifact_dir,"edit-invalid.txt",a.timeout),scenario(p,d,corpus,artifact_dir,"binary-safe.bin",a.timeout),scenario(p,d,corpus,artifact_dir,"code-1m.rs",a.timeout)]
            result.setdefault("diagnostics",[]).append(scenario(p,d,corpus,artifact_dir,"binary-render-repro.bin",a.timeout))
    statuses=[r["status"] for rs in result["profiles"].values() for r in rs]
    result["harness_status"]="PASS" if statuses and all(x=="PASS" for x in statuses) else "INCONCLUSIVE"
    diagnostics=result.get("diagnostics",[]); result["application_diagnostic_status"]="OBSERVED_FAILURE" if any(r.get("status")=="OBSERVED_FAILURE" for r in diagnostics) else ("INCONCLUSIVE" if diagnostics else "NOT_MEASURED")
    text=json.dumps(result,indent=2,sort_keys=True)+"\n"; print(text,end=""); Path(a.output).write_text(text) if a.output else None

def report(a):
    x=json.loads(Path(a.input).read_text())
    if x.get("phase")=="phase2":
        validate_phase2_result(x)
        text=render_phase2_report(x)
        validate_phase2_report(x,text)
        if a.output: Path(a.output).write_text(text)
        else: print(text,end="")
        return
    validate_result(x, Path(a.input).parent)
    assert all(v.get("status")=="NOT_MEASURED" for v in x.get("claims",{}).values())
    assert x.get("corpus_manifest_sha256") and x.get("profiles_meta") and x.get("metadata",{}).get("pty_limitation")
    for rs in x.get("profiles",{}).values():
        for r in rs: assert {"trace","stderr","endpoint_screen"}.issubset(r.get("evidence",{}))
    lines=["# Teddy S9 Phase 1 smoke","",f"Harness status: **{x['harness_status']}**","","PTY evidence is application emission/transport, not physical rendering.",""]
    for p,rs in x.get("profiles",{}).items():
        lines.append("## "+p); lines += [f"- `{r['scenario']}`: **{r['status']}** — {', '.join(r['reasons']) or 'completed'}" for r in rs]
    lines.append("\n## renderer diagnostic")
    lines += [f"- `{r['profile']}`: **{r['status']}** — {', '.join(r['reasons']) or 'observed'}" for r in x.get("diagnostics",[])]
    lines += ["","Claims C1–C5: `NOT_MEASURED`."]; text="\n".join(lines)+"\n"; Path(a.output).write_text(text) if a.output else print(text,end="")

def _artifact_path(e,artifact_root):
    path=e.get("path") if isinstance(e,dict) else None
    if not isinstance(path,str) or not path: raise ValueError("missing artifact path")
    p=Path(path)
    if not p.is_absolute(): p=REPO/p if path.startswith("bench/") else Path(artifact_root["path"])/p
    root=Path(artifact_root["path"]); root=root if root.is_absolute() else REPO/root
    try: p.resolve().relative_to(root.resolve())
    except ValueError: raise ValueError("artifact escapes declared root")
    return p

def _check_artifact(e,artifact_root):
    if not isinstance(e,dict) or len(e.get("sha256",""))!=64: raise ValueError("incomplete artifact evidence")
    p=_artifact_path(e,artifact_root)
    if not p.is_file() or sha(p)[0]!=e["sha256"]: raise ValueError(f"corrupt artifact: {e.get('path')}")
    return p

def _check_corpus_provenance(x):
    validate_config(CORPUS); config_digest,_=sha(ROOT/"corpora.json")
    if x.get("corpus_config_sha256")!=config_digest: raise ValueError("authoritative corpus config mismatch")
    manifest=ROOT/"corpus"/"manifest.json"; digest,_=sha(manifest)
    if x.get("corpus_manifest_sha256")!=digest: raise ValueError("authoritative corpus manifest mismatch")
    m=json.loads(manifest.read_text()); raw=json.dumps({k:v for k,v in m.items() if k!="aggregate_sha256"},sort_keys=True,separators=(",",":"),ensure_ascii=False).encode()
    if m.get("aggregate_sha256")!=hashlib.sha256(raw).hexdigest() or m.get("corpus_schema")!=CORPUS["schema"] or m.get("seed")!=CORPUS["seed"]: raise ValueError("invalid corpus aggregate provenance")
    actual={f["name"]:f for f in m.get("fixtures",[])}
    if set(actual)!=set(FIX): raise ValueError("corpus fixture provenance mismatch")
    for name,spec in FIX.items():
        data=fixture_bytes(name); entry=actual[name]
        if entry.get("size")!=len(data) or entry.get("sha256")!=hashlib.sha256(data).hexdigest() or entry.get("screen_markers")!=spec["markers"] or entry.get("post_edit_sha256")!=spec["post_edit_sha256"]: raise ValueError("corpus fixture provenance mismatch")

def _check_profile_provenance(x):
    meta=x.get("profiles_meta"); release=REPO/"target"/"release"
    if not isinstance(meta,dict) or set(meta)!={"bare","shipped"}: raise ValueError("missing staged profile metadata")
    for profile,names in (("bare",("teddy",)),("shipped",("teddy","teddy-highlight"))):
        info=meta[profile]
        if info.get("exact_files")!=sorted(names): raise ValueError("staged profile file set mismatch")
        for name in names:
            source=release/name; f=info.get("files",{}).get(name); src=info.get("identity_source",{}).get(name)
            if not source.is_file(): raise ValueError("missing staged source")
            h,z=sha(source)
            if not isinstance(f,dict) or f.get("sha256")!=h or f.get("size")!=z or not f.get("identity_verified"): raise ValueError("staged profile metadata mismatch")
            if not isinstance(src,dict) or src.get("sha256")!=h or src.get("size")!=z or src.get("arch")!=platform.machine(): raise ValueError("staged identity source mismatch")

def validate_result(x, output_dir=Path(".")):
    if x.get("phase")!=PHASE or x.get("schema")!=RESULT_SCHEMA: raise ValueError("wrong Phase 1 result schema")
    if set(x.get("claims",{}))!=set(CLAIMS) or any(v.get("status")!="NOT_MEASURED" for v in x.get("claims",{}).values()): raise ValueError("Phase 1 measured or missing claim")
    if x.get("application_status")!="NOT_MEASURED": raise ValueError("Phase 1 application status changed")
    if x.get("application_diagnostic_status")!="OBSERVED_FAILURE": raise ValueError("diagnostic status is not exact")
    ar=x.get("artifact_root",{})
    if not ar.get("path") or ar.get("resolvable_from")!="repository root": raise ValueError("missing artifact root")
    _check_corpus_provenance(x); _check_profile_provenance(x)
    required={"open-screen","page-down","insert-save-crlf","insert-save-invalid","read-only-binary"}
    if set(x.get("profiles",{}))!={"bare","shipped"}: raise ValueError("required profile matrix is not exactly 2x5")
    for profile,rs in x["profiles"].items():
        if len(rs)!=5 or {r.get("scenario") for r in rs}!=required: raise ValueError(f"{profile}: required scenario matrix is not exact")
        if any(r.get("status")!="PASS" for r in rs): raise ValueError(f"{profile}: required scenario is not PASS")
    if x.get("harness_status")!="PASS": raise ValueError("health/pass consistency failure")
    diagnostics=x.get("diagnostics",[])
    if len(diagnostics)!=2 or {r.get("profile") for r in diagnostics}!={"bare","shipped"} or any(r.get("status")!="OBSERVED_FAILURE" for r in diagnostics): raise ValueError("diagnostic matrix is not exact")
    required_records=[r for rs in x["profiles"].values() for r in rs]; records=required_records+diagnostics
    for r in records:
        for action in r.get("process",{}).get("actions",[]):
            if action.get("name")!="cleanup" and not valid_action_timing(action):
                raise ValueError("invalid action timestamp ordering/quiet interval")
        p=r.get("process",{}); identity=r.get("profile_identity",{})
        if r in required_records:
            readiness=p.get("readiness")
            if not isinstance(readiness,dict) or readiness.get("matched") is not True or readiness.get("matched_at") is None or not isinstance(readiness.get("snapshot"),list): raise ValueError("missing readiness endpoint/timing")
            if p.get("exit")!=0 or p.get("signal") is not None or not p.get("reaped"): raise ValueError("required lifecycle exit failure")
            if not p.get("pty_eof") or not p.get("stderr_eof") or not p.get("drain_complete") or p.get("drain_deadline"): raise ValueError("required lifecycle drain failure")
            if p.get("timed_out") or p.get("pty_output_capped") or p.get("stderr_capped") or p.get("unsupported") or p.get("cleanup_error"): raise ValueError("required lifecycle health failure")
            if p.get("pgid_after") or p.get("descendants_left") or not p.get("final_drain"): raise ValueError("required lifecycle PGID failure")
            if identity.get("identity_confidence")!="verified" or not identity.get("root_argv") or p.get("pgid_before_probe_error") or p.get("pgid_after_probe_error"): raise ValueError("required identity failure")
            expected_helper=identity.get("required_helper")
            if expected_helper != (r.get("profile")=="shipped"): raise ValueError("helper requirement mismatch")
            if r.get("profile")=="shipped" and not identity.get("helper_observed"): raise ValueError("shipped helper identity failure")
            expected=[identity["root_argv"]]+([identity["helper_argv"]] if expected_helper else [])
            try: actual=[shlex.split(m["command"]) for m in identity.get("live_process_group",[])]
            except (KeyError,ValueError): raise ValueError("malformed identity argv")
            if sorted(actual)!=sorted(expected): raise ValueError("staged process argv identity mismatch")
            if r.get("scenario")=="page-down":
                rows=r.get("endpoint",{}).get("snapshot",{}).get("rows",[]); bounds=FIX["code-1m.rs"]["page_rows"]
                if len(rows)<=bounds[1] or not rows[bounds[0]].startswith("S9_ROW_00001") or not rows[bounds[1]].startswith("S9_ROW_00048"): raise ValueError("PageDown endpoint rows mismatch")
    for r in records:
        pinfo=r.get("process",{}); ev=r.get("evidence",{}); tr=ev.get("trace",{})
        if not isinstance(tr,dict) or tr.get("full_bytes")!=pinfo.get("trace_bytes") or tr.get("retained_bytes")!=pinfo.get("trace_retained_bytes") or tr.get("discarded_bytes")!=pinfo.get("trace_discarded_bytes") or tr.get("full_sha256")!=pinfo.get("trace_sha256"): raise ValueError("mismatched full trace evidence")
    for r in records:
        for item in ("trace","stderr","endpoint_screen"):
            _check_artifact(r.get("evidence",{}).get(item),ar)
        endpoint=json.loads(_check_artifact(r["evidence"]["endpoint_screen"],ar).read_text())
        if endpoint!=r.get("endpoint"): raise ValueError("endpoint artifact does not equal result snapshot")
        rows=endpoint.get("snapshot",{}).get("rows") if isinstance(endpoint.get("snapshot"),dict) else None
        if r in required_records and not isinstance(rows,list): raise ValueError("missing endpoint rows")
        if r in required_records and r.get("scenario")=="page-down":
            bounds=FIX["code-1m.rs"]["page_rows"]
            if len(rows)<=bounds[1] or not rows[bounds[0]].startswith("S9_ROW_00001") or not rows[bounds[1]].startswith("S9_ROW_00048"): raise ValueError("PageDown endpoint rows mismatch")
        if r in required_records and r.get("scenario") in ("insert-save-crlf","insert-save-invalid") and (len(rows)<2 or not rows[1].startswith("X")): raise ValueError("edit endpoint rows mismatch")
        if r in required_records and r.get("scenario")=="read-only-binary" and not valid_binary_evidence(rows,FIX["binary-safe.bin"]): raise ValueError("binary endpoint evidence mismatch")
    for r in diagnostics:
        p=r.get("process",{})
        identity=r.get("profile_identity",{}); expected=[identity.get("root_argv")]+([identity.get("helper_argv")] if identity.get("required_helper") else [])
        try: actual=[shlex.split(m["command"]) for m in identity.get("live_process_group",[])]
        except (KeyError,ValueError): raise ValueError("diagnostic malformed identity")
        identity_bad=identity.get("identity_confidence")!="verified" or sorted(actual)!=sorted(expected) or identity.get("helper_observed") != (r.get("profile")=="shipped") or (r.get("profile")=="shipped" and identity.get("required_helper") is not True)
        if p.get("exit")!=101 or p.get("signal") is not None or not p.get("reaped") or not p.get("drain_complete") or p.get("drain_deadline") or p.get("cleanup_error") or not p.get("pty_eof") or not p.get("stderr_eof") or p.get("timed_out") or p.get("pty_output_capped") or p.get("stderr_capped") or p.get("unsupported") or p.get("pgid_after") or p.get("descendants_left") or p.get("pgid_before_probe_error") or p.get("pgid_after_probe_error") or identity_bad or "src/render.rs:229:57" not in p.get("stderr",""):
            raise ValueError("diagnostic lifecycle/signature failure")
    return True

def phase2_quantiles(values):
    if not values: return {"count":0,"p50":None,"p95":None}
    v=sorted(values)
    def q(f):
        pos=(len(v)-1)*f; lo=int(pos); hi=min(lo+1,len(v)-1); return v[lo]+(v[hi]-v[lo])*(pos-lo)
    return {"count":len(v),"p50":q(.50),"p95":q(.95)}

def phase2_environment():
    return {k:os.environ[k] for k in sorted(PHASE2_ENV_ALLOWLIST) if k in os.environ}

def parse_perf_log(raw):
    rows=[]
    for line in raw.splitlines():
        m=re.fullmatch(r"\s*([0-9]+(?:\.[0-9]+)?)\s+([0-9]+)\s+([0-9]+)\s*",line)
        if not m: continue
        rows.append({"us":float(m.group(1)),"allocs":int(m.group(2)),"frame_bytes":int(m.group(3))})
    return rows

def _phase2_file(path,root):
    p=Path(path); rel=str(p.relative_to(REPO)) if p.is_relative_to(REPO) else str(p.resolve()); h,z=sha(p); return {"path":rel,"sha256":h,"size":z}

def generate_phase2_log(path):
    path=Path(path); path.parent.mkdir(parents=True,exist_ok=True); total=1<<30; line=b"S9_C1_ROW_000000 source_like_phase2_fixed_line_000000000000000000000000000\n"; line=line[:64].ljust(63,b" ")+b"\n"; tail=b"S9_C1_TAIL".ljust(63,b" ")+b"\n"; count=(total-len(tail))//len(line); h=hashlib.sha256(); written=0
    with path.open("wb") as f:
        block=line*4096
        for _ in range(count//4096): f.write(block); h.update(block); written+=len(block)
        for _ in range(count%4096): f.write(line); h.update(line); written+=len(line)
        f.write(tail); h.update(tail); written+=len(tail)
    if written!=total: raise RuntimeError("large log size mismatch")
    return {"size":written,"sha256":h.hexdigest(),"head":"S9_C1_ROW_000000","tail":"S9_C1_TAIL","line_bytes":64}

def _phase2_profiles(root,built):
    profiles={}
    for name,names in (("bare",("teddy",)),("shipped",("teddy","teddy-highlight"))):
        d=Path(root)/name; d.mkdir(parents=True,exist_ok=True); files={}
        for n in names:
            src=Path(built)/n; dst=d/n; shutil.copy2(src,dst); h,z=sha(dst); vp=subprocess.run([str(dst),"--version"],capture_output=True,text=True,timeout=2); files[n]={"sha256":h,"size":z,"version":vp.stdout.strip(),"version_capture":{"stdout":vp.stdout,"stderr":vp.stderr,"exit":vp.returncode},"arch":platform.machine(),"path":str(dst)}
        profiles[name]={"root":str(d/"teddy"),"helper":str(d/"teddy-highlight") if name=="shipped" else None,"files":files,"exact_files":sorted(names)}
    return profiles

def _rss_snapshot(pgid,members):
    rows=[]; total=0
    for m in members:
        pid=m.get("pid")
        try:
            raw=subprocess.check_output(["ps","-p",str(pid),"-o","rss=,pid=,pgid=,command="],text=True,stderr=subprocess.DEVNULL).strip(); parts=raw.split(None,3); rss=int(parts[0]); total+=rss*1024; rows.append({"raw":raw,"pid":pid,"rss_bytes":rss*1024})
        except Exception: pass
    return {"at":time.monotonic(),"pgid":pgid,"members":rows,"rss_bytes":total}

def _perf_attempt(profile,run):
    tag=profile["root"].split("/")[-2]; log=Path(run)/("c2-"+tag+".log"); binary=profile["root"]; s=Session([binary,str(ROOT/"corpus"/"code-1m.rs")],timeout=4); s.extra_env={"TEDDY_PERF":str(log)}; home=Path(tempfile.mkdtemp(prefix="s9-p2-perf-")); s.spawn(home,home); ready=s.ready("code-1m.rs","S9_ROW_00000"); action=s.write(b"\033[6~",lambda sc:sc.contains("S9_ROW_00001"),"perf_page") if ready else False; clean=s.close(); log.touch(); rows=parse_perf_log(log.read_text()); p=Path(run)/("c2-"+tag+"-attempt.json"); rec={"profile":tag,"ready":ready,"action":action,"clean":clean,"process":s.record(),"log":_phase2_file(log,REPO),"parsed_rows":rows,"line_count":len(log.read_text().splitlines()),"association":"none"}; p.write_text(json.dumps(rec,sort_keys=True,indent=2)+"\n"); shutil.rmtree(home,ignore_errors=True); return rec,_phase2_file(p,REPO)

def _c3_attempt(profile,large,run):
    s=Session([profile["root"],str(large)],timeout=4); home=Path(tempfile.mkdtemp(prefix="s9-p2-search-")); s.spawn(home,home); ready=s.ready(large.name,"S9_C1_ROW_000000"); needle=b"S9_C1_TAIL"; prompt=None; search=None; cancel=None
    if ready:
        prompt=s.write(b"\x06",lambda sc:sc.contains("Find") or sc.contains("find") or sc.contains("SEARCH"),"search_prompt")
        if prompt:
            typed=s.write(needle,lambda sc:sc.contains("S9_C1_TAIL"),"literal_find")
            search=s.write(b"\r",lambda sc:not sc.contains("Find") and not sc.contains("find") and sc.contains(large.name),"search_submit") if typed else False
        cancel=s.write(b"\x1b",lambda sc:sc.contains(large.name) and sc.contains("S9_C1_ROW_000000"),"search_cancel")
    clean=s.close(); tag=profile["root"].split("/")[-2]; p=Path(run)/("c3-"+tag+"-attempt.json"); rec={"profile":tag,"needle":needle.decode(),"keys":{"start_search":"Ctrl-F","submit":"Enter","cancel":"Escape"},"ready":ready,"prompt_observed":prompt,"search_observed":search,"cancel_observed":cancel,"actions":[a for a in s.record()["actions"]],"clean":clean,"process":s.record(),"semantic_association":bool(prompt and search and cancel)}; p.write_text(json.dumps(rec,sort_keys=True,indent=2)+"\n"); shutil.rmtree(home,ignore_errors=True); return rec,_phase2_file(p,REPO)

COMPARATOR_NAMES=("nvim","vim","vi","hx","kak","less","vis")
COMPARATOR_RUN_NAMES=("nvim","vim","hx","kak","less")
_LESS_ENV_CLEAR=("LESS","MORE","LESSKEY","LESSKEY_SYSTEM","LESSKEYIN","LESSKEYIN_SYSTEM","LESSOPEN","LESSCLOSE")

def comparator_argv(name,path,corpus):
    path=str(Path(path).resolve()); corpus=str(Path(corpus).resolve())
    if name=="nvim": return [path,"--clean","-R","--",corpus]
    if name in ("vim","vi"): return [path,"--clean","-R","-i","NONE","-U","NONE","--",corpus]
    if name=="hx": return [path,"--config","/dev/null","--",corpus]
    if name=="kak": return [path,"-n","-ro","-ui","terminal","--",corpus]
    if name=="less": return [path,"-n","-L","--",corpus]
    raise ValueError("unsupported comparator")

def comparator_environment(home,name):
    env=isolated_env(home)
    if name=="less":
        env["LESSHISTFILE"]="-"
        for key in _LESS_ENV_CLEAR: env.pop(key,None)
    return env

def discover_comparators():
    out=[]; seen={}
    for name in COMPARATOR_NAMES:
        path=shutil.which(name) or ("/usr/bin/vis" if name=="vis" and Path("/usr/bin/vis").exists() else None)
        if not path: out.append({"name":name,"status":"UNAVAILABLE"}); continue
        resolved=str(Path(path).resolve())
        if resolved in seen:
            out.append({"name":name,"status":"ALIAS_OF","alias_of":seen[resolved],"path":resolved}); continue
        seen[resolved]=name; h,z=sha(path)
        try:
            probe_arg="-version" if name=="kak" else "--version"
            probe=subprocess.run([resolved,probe_arg],capture_output=True,text=True,timeout=2)
            status="REJECTED_UNSUPPORTED" if resolved=="/usr/bin/vis" else ("IDENTITY_RECORDED" if probe.returncode==0 else "UNSUPPORTED")
            version={"stdout":probe.stdout,"stderr":probe.stderr,"exit":probe.returncode}
        except Exception as e:
            status="REJECTED_UNSUPPORTED" if resolved=="/usr/bin/vis" else "UNSUPPORTED"; version={"stdout":"","stderr":type(e).__name__,"exit":None}
        out.append({"name":name,"status":status,"path":resolved,"sha256":h,"size":z,"version":version})
    return out

def _comparator_quit(s,name):
    sequences={"nvim":b":q!\r","vim":b":q!\r","vi":b":q!\r","hx":b":q!\r","kak":b":q\r","less":b"q"}
    try: os.write(s.master,sequences[name])
    except OSError: pass
    deadline=time.monotonic()+1.5
    while s.pid>0 and time.monotonic()<deadline:
        s._read(.01); s.poll()

def _comparator_attempt(tool,corpus,run,index):
    name=tool["name"]; argv=comparator_argv(name,tool["path"],corpus); home=Path(tempfile.mkdtemp(prefix="s9-comparator-home-")); s=Session(argv,timeout=6); s.extra_env=comparator_environment(home,name); s.spawn(home,home); identity=exact_group_identity(process_group(s.original_pgid),argv); ready=s.until(lambda sc:sc.contains("S9_C1_ROW_000000"),"comparator_ready"); s.readiness=ready; _comparator_quit(s,name); clean=s.close(); process=s.record(); screen=Path(run)/(f"comparator-{name}-{index}-screen.txt"); trace=Path(run)/(f"comparator-{name}-{index}-trace.bin"); screen.write_text("\n".join(process["screen"])+"\n"); trace.write_bytes(s.trace); screen_evidence=_phase2_file(screen,REPO); trace_evidence=_phase2_file(trace,REPO); valid=bool(ready.get("matched")) and identity[0] and clean and process.get("exit")==0 and process.get("reaped") and process.get("drain_complete") and not process.get("drain_deadline") and not process.get("cleanup_error") and not process.get("pgid_after") and not process.get("descendants_left") and not process.get("timed_out") and not process.get("pty_output_capped") and not process.get("stderr_capped") and not process.get("unsupported"); rec={"name":name,"rep":index,"status":"PASS" if valid else "INCONCLUSIVE","argv":argv,"invocation_class":name,"identity":identity[0],"readiness":ready,"elapsed_ms":ready.get("matched_at")*1000 if ready.get("matched_at") is not None else None,"process":process,"screen":screen_evidence,"trace":trace_evidence}; raw=Path(run)/(f"comparator-{name}-{index}-attempt.json"); raw.write_text(json.dumps(rec,sort_keys=True,indent=2)+"\n"); rec["artifact"]=_phase2_file(raw,REPO); shutil.rmtree(home,ignore_errors=True); return rec

def run_comparators(tools,corpus,run):
    records=[]; evidence=[]
    for tool in tools:
        common={"name":tool["name"],"path":tool.get("path"),"alias_of":tool.get("alias_of"),"discovery_status":tool["status"],"invocation_class":tool["name"]}
        if tool["name"] not in COMPARATOR_RUN_NAMES or tool.get("status")!="IDENTITY_RECORDED":
            records.append({**common,"status":tool["status"],"attempts":[]}); continue
        attempts=[_comparator_attempt(tool,corpus,run,i) for i in range(1,3)]; q=phase2_quantiles([a["elapsed_ms"] for a in attempts if a.get("elapsed_ms") is not None]); status="PASS" if all(a["status"]=="PASS" for a in attempts) else "INCONCLUSIVE"; rec={**common,"status":status,"argv":comparator_argv(tool["name"],tool["path"],corpus),"attempts":attempts,"quantiles_ms":q}; raw=Path(run)/(f"comparator-{tool['name']}.json"); raw.write_text(json.dumps(rec,sort_keys=True,indent=2)+"\n"); rec["artifact"]=_phase2_file(raw,REPO); records.append(rec); evidence.append(rec["artifact"]); evidence.extend(a["artifact"] for a in attempts); evidence.extend(a[k] for a in attempts for k in ("screen","trace"))
    return records,evidence

def _comparator_attempt_status(attempt, expected_argv):
    """Derive, rather than trust, the status of one comparator observation."""
    process=attempt.get("process")
    readiness=attempt.get("readiness")
    if not isinstance(process,dict) or not isinstance(readiness,dict): return "INCONCLUSIVE"
    observed=process.get("identity")
    snapshot=readiness.get("snapshot")
    matched_endpoint=(isinstance(snapshot,list) and
                      any(isinstance(line,str) and "S9_C1_ROW_000000" in line
                          for line in snapshot))
    identity=(attempt.get("identity") is True and
              observed.get("argv") == expected_argv and observed.get("verified") is True
              if isinstance(observed,dict) else False)
    timing=(isinstance(attempt.get("elapsed_ms"),(int,float)) and
            math.isfinite(attempt["elapsed_ms"]) and attempt["elapsed_ms"] >= 0 and
            isinstance(readiness.get("matched_at"),(int,float)) and
            math.isfinite(readiness["matched_at"]) and
            attempt["elapsed_ms"] == readiness["matched_at"]*1000)
    lifecycle=(readiness.get("matched") is True and readiness.get("name")=="comparator_ready" and
               matched_endpoint and identity and timing and
               process.get("exit")==0 and process.get("signal") is None and
               process.get("reaped") is True and process.get("pty_eof") is True and
               process.get("stderr_eof") is True and process.get("drain_complete") is True and
               not process.get("drain_deadline") and not process.get("cleanup_error") and
               not process.get("pgid_after") and not process.get("descendants_left") and
               not process.get("timed_out") and not process.get("pty_output_capped") and
               not process.get("stderr_capped") and not process.get("unsupported") and
               process.get("exec_failed") is False)
    return "PASS" if lifecycle else "INCONCLUSIVE"

def _phase2_rep(profile,large,root,run_id,index):
    home=Path(tempfile.mkdtemp(prefix="s9-p2-home-")); copy=Path(large); s=Session([profile["root"],str(copy)],timeout=8); s.spawn(home,home,preflight=False); expected=[str(profile["root"]),str(copy)]
    ready=s.until(lambda sc:sc.alt and sc.contains(copy.name) and sc.contains("S9_C1_ROW_000000"),"c1_ready"); s.readiness=ready
    observer_boundary_ms=ready.get("matched_at")*1000 if ready.get("matched") else None
    if ready.get("matched"):
        identity_started=time.monotonic(); identity=s._ps(); identity_finished=time.monotonic(); s.identity=identity; s.identity_probe_started_ms=(identity_started-s.t0)*1000; s.identity_probe_finished_ms=(identity_finished-s.t0)*1000; s.identity_probe_ms=s.identity_probe_finished_ms-s.identity_probe_started_ms
        group_started=time.monotonic(); ok,members,error=process_group(s.original_pgid); group_finished=time.monotonic(); s.pgid_before=list(members); s.pgid_before_probe_error=error; s.group_probe_started_ms=(group_started-s.t0)*1000; s.group_probe_finished_ms=(group_finished-s.t0)*1000; s.group_probe_ms=s.group_probe_finished_ms-s.group_probe_started_ms
    else:
        identity={"verified":False,"deferred":True}; s.identity=identity; s.identity_probe_started_ms=s.identity_probe_finished_ms=s.identity_probe_ms=None; ok,members,error=False,[],"readiness endpoint missing"; s.pgid_before_probe_error=error; s.group_probe_started_ms=s.group_probe_finished_ms=s.group_probe_ms=None
    identity_result=exact_group_identity((ok,members,error),expected,[profile["helper"]] if profile["helper"] else None); rss=[_rss_snapshot(s.original_pgid,members),_rss_snapshot(s.original_pgid,members)]; quiet=s.quiet() if ready.get("matched") else False; clean=s.close(); valid=bool(ready.get("matched")) and quiet and clean and ok and identity_result[0] and s.drain_complete and not s.pgid_after and not s.descendants and not s.timed_out and not s.output_capped and not s.screen.unsupported; elapsed=ready.get("matched_at") if ready.get("matched") else None
    rec={"profile":profile["root"].split("/")[-2],"rep":index,"status":"PASS" if valid else "INCONCLUSIVE","elapsed_ms":elapsed*1000 if elapsed is not None else None,"identity":identity_result[0],"observer_boundary":PHASE2_METHODOLOGY,"observer_boundary_ms":observer_boundary_ms,"identity_probe":{"started_ms":s.identity_probe_started_ms,"finished_ms":s.identity_probe_finished_ms,"duration_ms":s.identity_probe_ms,"verified":identity.get("verified") is True},"process_group_probe":{"started_ms":s.group_probe_started_ms,"finished_ms":s.group_probe_finished_ms,"duration_ms":s.group_probe_ms,"success":ok,"identity":list(identity_result)},"rss_samples":rss,"rss_peak_bytes":max((x["rss_bytes"] for x in rss),default=0),"process":s.record()}; p=Path(root)/(rec["profile"]+f"-c1-{index}.json"); p.write_text(json.dumps(rec,sort_keys=True,indent=2)+"\n"); rec["artifact"]=_phase2_file(p,REPO); shutil.rmtree(home,ignore_errors=True); return rec

def validate_phase2_result(x):
    if x.get("schema")!=PHASE2_SCHEMA or x.get("phase")!="phase2": raise ValueError("wrong Phase 2 schema")
    if x.get("methodology")!=PHASE2_METHODOLOGY or x.get("historical_context")!=PHASE2_HISTORICAL_CONTEXT: raise ValueError("wrong Phase 2 methodology context")
    if set(x.get("claims",{}))!=set("C1 C2 C3 C4 C5".split()): raise ValueError("missing claims")
    allowed={"PASS","FAIL","INCONCLUSIVE","NOT_MEASURED"}
    if any(v.get("status") not in allowed or not v.get("reason") for v in x["claims"].values()): raise ValueError("invalid claim outcome")
    if not x.get("artifact_root") or not x["artifact_root"].get("path"): raise ValueError("missing Phase 2 artifact root")
    for e in x.get("evidence",[]):
        p=_check_artifact(e,x["artifact_root"])
        if not p.is_file(): raise ValueError("missing Phase 2 evidence")
    c1=x["claims"]["C1"]
    if c1.get("methodology")!=PHASE2_METHODOLOGY or set(c1.get("profiles",{}))!={"bare","shipped"} or any(len(rs)!=5 for rs in c1["profiles"].values()): raise ValueError("C1 repetition matrix mismatch")
    for rs in c1["profiles"].values():
        for r in rs:
            boundary=r.get("observer_boundary_ms"); ip=r.get("identity_probe"); gp=r.get("process_group_probe")
            if r.get("observer_boundary")!=PHASE2_METHODOLOGY or not isinstance(boundary,(int,float)) or r.get("elapsed_ms")!=boundary or not isinstance(ip,dict) or not isinstance(gp,dict) or not isinstance(ip.get("started_ms"),(int,float)) or not isinstance(gp.get("started_ms"),(int,float)) or ip["started_ms"]<boundary or gp["started_ms"]<boundary: raise ValueError("C1 observer boundary violation")
            if r.get("status")=="PASS":
                p=r.get("process",{})
                if not r.get("identity") or p.get("exit")!=0 or p.get("signal") is not None or not p.get("reaped") or not p.get("pty_eof") or not p.get("stderr_eof") or not p.get("drain_complete") or p.get("drain_deadline") or p.get("cleanup_error") or p.get("pgid_after") or p.get("descendants_left") or p.get("timed_out") or p.get("pty_output_capped") or p.get("stderr_capped") or p.get("unsupported"): raise ValueError("C1 PASS lifecycle failure")
            if not isinstance(r.get("artifact"),dict): raise ValueError("missing C1 repetition evidence")
    c1_artifacts=[]
    for profile,rs in c1["profiles"].items():
        for r in rs:
            ap=_check_artifact(r["artifact"],x["artifact_root"]); a=json.loads(ap.read_text())
            if any(a.get(k)!=r.get(k) for k in ("profile","rep","status","elapsed_ms","identity","observer_boundary","observer_boundary_ms","identity_probe","process_group_probe","process","rss_samples","rss_peak_bytes")): raise ValueError("C1 repetition artifact mismatch")
            c1_artifacts.append(a)
    derived={p:phase2_quantiles([a["elapsed_ms"] for a in c1_artifacts if a["profile"]==p and a.get("status")=="PASS"]) for p in c1["profiles"]}
    if c1.get("quantiles_ms")!=derived: raise ValueError("C1 forged quantiles")
    all_c1_valid=all(all(r.get("status")=="PASS" for r in rs) for rs in c1["profiles"].values())
    threshold_ok=all(derived[p].get("p95") is not None and derived[p]["p95"]<50 for p in c1["profiles"])
    expected_c1_status="INCONCLUSIVE" if not all_c1_valid else ("PASS" if threshold_ok else "FAIL")
    if c1.get("status")!=expected_c1_status: raise ValueError("C1 threshold/status inconsistency")
    if x["claims"]["C5"]["status"]!="NOT_MEASURED": raise ValueError("C5 must remain NOT_MEASURED")
    c2=x["claims"]["C2"]
    if len(c2.get("attempts",[]))!=2: raise ValueError("missing C2 runtime evidence")
    c2_paths={e.get("path"):e for e in x.get("evidence",[]) if isinstance(e,dict)}
    c2_rows=[]; c2_complete=True
    for a in c2["attempts"]:
        if not isinstance(a.get("log"),dict) or not isinstance(a.get("process"),dict) or "ready" not in a or "action" not in a or not isinstance(a.get("parsed_rows"),list) or not a.get("parsed_rows") or a.get("line_count")!=len(a["parsed_rows"]): raise ValueError("missing C2 runtime evidence")
        log_path=_check_artifact(a["log"],x["artifact_root"])
        if parse_perf_log(log_path.read_text())!=a["parsed_rows"]: raise ValueError("C2 log/attempt mismatch")
        attempt_path=next((p for p in c2_paths if isinstance(p,str) and p.endswith(f"c2-{a.get('profile')}-attempt.json")),None)
        if not attempt_path: raise ValueError("missing C2 attempt artifact")
        attempt_raw=json.loads(_check_artifact(c2_paths[attempt_path],x["artifact_root"]).read_text())
        if attempt_raw!=a: raise ValueError("C2 attempt artifact mismatch")
        p=a["process"]
        lifecycle=p.get("exit")==0 and p.get("reaped") and p.get("drain_complete") and not p.get("pgid_after") and not p.get("descendants_left") and not p.get("timed_out")
        action=next((z for z in p.get("actions",[]) if z.get("name")=="perf_page"),None)
        associated=a.get("association")=="action"
        c2_complete = c2_complete and bool(a["ready"] and a["action"] and a["clean"] and lifecycle and action and action.get("endpoint_found") and associated)
        c2_rows.extend(a["parsed_rows"])
    c2_quantiles=phase2_quantiles([r["us"] for r in c2_rows])
    if c2.get("quantiles_us")!=c2_quantiles: raise ValueError("C2 forged quantiles")
    expected_c2_status=("PASS" if c2_quantiles.get("p95") is not None and c2_quantiles["p95"]<1000 else "FAIL") if c2_complete else "INCONCLUSIVE"
    if c2.get("status")!=expected_c2_status: raise ValueError("C2 threshold/status inconsistency")
    c3=x["claims"]["C3"]
    if len(c3.get("attempts",[]))!=2 or any(not isinstance(a.get("process"),dict) or not a.get("needle") or not a.get("actions") or [z.get("name") for z in a["actions"]]!=["search_prompt","literal_find","search_submit","search_cancel"] or [z.get("bytes") for z in a["actions"]]!=["06",a["needle"].encode().hex(),"0d","1b"] or (a.get("semantic_association") and not (a.get("search_observed") and a.get("cancel_observed"))) for a in c3["attempts"]): raise ValueError("missing C3 runtime evidence")
    c3_paths={e.get("path"):e for e in x.get("evidence",[]) if isinstance(e,dict)}
    for a in c3["attempts"]:
        ep=next((e for p,e in c3_paths.items() if p and p.endswith("c3-"+a["profile"]+"-attempt.json")),None)
        if not ep: raise ValueError("missing C3 attempt artifact")
        raw=json.loads(_check_artifact(ep,x["artifact_root"].copy()).read_text())
        if raw!=a: raise ValueError("C3 attempt artifact mismatch")
        if a["process"].get("exit")!=0 or not a["process"].get("reaped") or not a["process"].get("drain_complete") or a["process"].get("pgid_after") or a["process"].get("descendants_left"): raise ValueError("C3 lifecycle failure")
        for action in a["actions"]:
            if action["name"]!="search_cancel" and not valid_action_timing(action): raise ValueError("C3 causal action timing failure")
            if action["name"]=="search_cancel" and not (action.get("write")<action.get("first_output_after_write")<=action.get("last_output")<=action.get("quiet_complete")): raise ValueError("C3 cancellation timing failure")
        if a.get("semantic_association") and (not a.get("search_observed") or not a.get("cancel_observed") or not next(z for z in a["actions"] if z["name"]=="search_submit").get("endpoint_found") or not next(z for z in a["actions"] if z["name"]=="search_cancel").get("endpoint_found")): raise ValueError("C3 forged semantic association")
    if c3.get("status") not in {"PASS","INCONCLUSIVE"}: raise ValueError("invalid C3 outcome")
    c3_complete=all(a.get("semantic_association") for a in c3["attempts"])
    expected_c3_status="PASS" if c3_complete else "INCONCLUSIVE"
    if c3.get("status")!=expected_c3_status: raise ValueError("C3 association/status inconsistency")
    samples=x["claims"]["C5"].get("samples",[])
    if not isinstance(samples,list) or len(samples)<10 or not x["claims"]["C5"].get("evidence") or any(not isinstance(s.get("samples"),list) or len(s["samples"])<2 or any(not isinstance(q.get("at"),(int,float)) or not isinstance(q.get("rss_bytes"),int) or not isinstance(q.get("members"),list) for q in s["samples"]) for s in samples): raise ValueError("missing C5 RSS evidence")
    if set(x.get("environment",{}))-PHASE2_ENV_ALLOWLIST: raise ValueError("non-allowlisted Phase 2 environment key")
    for info in x.get("profiles",{}).values():
        for f in info.get("files",{}).values():
            vc=f.get("version_capture")
            if not isinstance(vc,dict) or not isinstance(vc.get("stdout"),str) or not isinstance(vc.get("stderr"),str) or not isinstance(vc.get("exit"),int): raise ValueError("missing teddy version capture")
    for r in x["claims"]["C4"].get("fixtures",[]):
        saved=r.get("saved_output")
        if not isinstance(saved,dict) or not r.get("actual_saved_sha256") or not r.get("expected") or not r.get("source_unchanged"): raise ValueError("C4 missing actual saved digest evidence")
        saved_path=_check_artifact(saved,x["artifact_root"])
        computed=sha(saved_path)[0]
        digest_ok=computed==saved.get("sha256")==r.get("actual_saved_sha256")==r.get("expected")
        expected_fixture_status="PASS" if r.get("source_unchanged") and digest_ok else "FAIL"
        if r.get("status")!=expected_fixture_status: raise ValueError("C4 fixture/status inconsistency")
    if len(x["claims"]["C4"].get("fixtures",[]))!=4 or { (r.get("profile"),r.get("fixture")) for r in x["claims"]["C4"].get("fixtures",[]) } != { (p,n) for p in ("bare","shipped") for n in ("edit-crlf.txt","edit-invalid.txt") }: raise ValueError("C4 matrix is not exact")
    c4=x["claims"]["C4"]
    if any(r.get("status") not in {"PASS","FAIL"} for r in c4.get("fixtures",[])): raise ValueError("invalid C4 fixture outcome")
    expected_c4_status="PASS" if all(r.get("status")=="PASS" for r in c4["fixtures"]) else "FAIL"
    if c4.get("status")!=expected_c4_status: raise ValueError("C4 matrix/status inconsistency")
    tools=x.get("comparators",{}).get("tools",[])
    expected_names=set(COMPARATOR_NAMES)
    if x.get("comparators",{}).get("status") not in {"DISCOVERED","UNAVAILABLE","SUPPORTED","REJECTED"} or not isinstance(tools,list) or len(tools)!=7 or {t.get("name") for t in tools}!=expected_names: raise ValueError("invalid comparator outcome")
    if len({t.get("name") for t in tools})!=7: raise ValueError("duplicate comparator identity")
    vis=next(t for t in tools if t.get("name")=="vis")
    if vis.get("path")=="/usr/bin/vis" and vis.get("status")!="REJECTED_UNSUPPORTED": raise ValueError("/usr/bin/vis not rejected")
    records=x.get("comparators",{}).get("records")
    if records is not None:
        if not isinstance(records,list) or len(records)!=7 or {r.get("name") for r in records}!={t.get("name") for t in tools}: raise ValueError("comparator record matrix mismatch")
        for r in records:
            tool=next(t for t in tools if t.get("name")==r.get("name"))
            if r.get("path")!=tool.get("path") or r.get("alias_of")!=tool.get("alias_of") or r.get("discovery_status")!=tool.get("status") or r.get("invocation_class")!=r.get("name"): raise ValueError("comparator discovery/record identity mismatch")
            if r.get("status") not in {"PASS","INCONCLUSIVE","UNAVAILABLE","UNSUPPORTED","REJECTED_UNSUPPORTED","ALIAS_OF"}: raise ValueError("invalid comparator status")
            if r.get("status") in {"UNAVAILABLE","UNSUPPORTED","REJECTED_UNSUPPORTED","ALIAS_OF"}:
                if r.get("status")!=tool.get("status"): raise ValueError("inactive comparator status mismatch")
                if r.get("attempts"): raise ValueError("unsupported comparator has attempts")
                continue
            if len(r.get("attempts",[]))!=2 or not isinstance(r.get("artifact"),dict): raise ValueError("missing comparator attempts")
            canonical_corpus=str(_check_artifact(x["corpus"]["evidence"],x["artifact_root"]).resolve())
            if r.get("discovery_status")!="IDENTITY_RECORDED" or r.get("argv")!=comparator_argv(r["name"],tool["path"],canonical_corpus): raise ValueError("comparator argv identity mismatch")
            rp=_check_artifact(r["artifact"],x["artifact_root"]); raw=json.loads(rp.read_text())
            if raw!={k:v for k,v in r.items() if k!="artifact"}: raise ValueError("comparator record artifact mismatch")
            for a in r["attempts"]:
                if a.get("status") not in {"PASS","INCONCLUSIVE"} or not isinstance(a.get("process"),dict): raise ValueError("invalid comparator attempt")
                ap=_check_artifact(a["artifact"],x["artifact_root"]); ar=json.loads(ap.read_text())
                if ar!={k:v for k,v in a.items() if k!="artifact"}: raise ValueError("comparator attempt artifact mismatch")
                _check_artifact(a["screen"],x["artifact_root"]); _check_artifact(a["trace"],x["artifact_root"])
                expected_attempt=_comparator_attempt_status(a,comparator_argv(r["name"],tool["path"],canonical_corpus))
                if a.get("status")!=expected_attempt: raise ValueError("comparator attempt status mismatch")
            expected_status="PASS" if all(_comparator_attempt_status(a,comparator_argv(r["name"],tool["path"],canonical_corpus))=="PASS" for a in r["attempts"]) else "INCONCLUSIVE"
            if r.get("status")!=expected_status: raise ValueError("comparator aggregate/status mismatch")
            expected_quantiles=phase2_quantiles([a["elapsed_ms"] for a in r["attempts"] if isinstance(a.get("elapsed_ms"),(int,float))])
            if r.get("quantiles_ms")!=expected_quantiles: raise ValueError("comparator forged quantiles")
    return True

def phase2_report_markdown(result):
    c=result["claims"]; runtime=result["profiles"]["bare"]["files"]["teddy"]["version_capture"]; lines=["# S9 Phase 2 benchmark results","","## Methodology context",f"- Methodology: `{result['methodology']}`; C1 identity/PGID observation is deferred until after the named readiness sentinel.",f"- Historical context: {result['historical_context']}","- C1 elapsed time starts at the post-fork harness clock; it is not complete process-launch latency.","","## Executive summary",f"Run: `{result['command']}`",f"Source: `{result['source']['commit']}` (dirty: `{result['source']['dirty']}`)",f"Cargo package: `{result['cargo_package_version']}`",f"Runtime `teddy --version`: `{runtime['stdout'].strip()}` (exit `{runtime['exit']}`)",f"Artifact bundle: `{result['artifact_root']['path']}`","","## Claim summary","","| Claim | Status | Criterion / limitation | Evidence |","|---|---|---|---|"]
    criteria={"C1":"p95 < 50 ms for both profiles; post-readiness observer boundary","C2":"action-associated perf p95 < 1000 us","C3":"observable literal search and cancellation","C4":"source unchanged and X+original digest","C5":"report-only RSS; no budget"}
    for k in "C1 C2 C3 C4 C5".split():
        evidence=c[k].get("evidence") or ("C1 repetition artifacts" if k=="C1" else "fixture evidence" if k=="C4" else "runtime attempt artifacts")
        ep=evidence if isinstance(evidence,str) else (evidence.get("path") if isinstance(evidence,dict) else "runtime artifacts")
        lines.append(f"| {k} | **{c[k]['status']}** | {criteria[k]}; {c[k]['reason']} | `{ep}` |")
    lines += ["","## C1 repetitions","","| Profile | Repetitions | p50 (ms) | p95 (ms) |","|---|---:|---:|---:|"]
    for p,q in c["C1"]["quantiles_ms"].items(): lines.append(f"| {p} | {q['count']} | {q['p50']:.3f} | {q['p95']:.3f} |")
    fmt=lambda value: f"{value:.3f}" if isinstance(value,(int,float)) else "unavailable"
    lines += ["","## Editor metric summary","","| Editor / profile | Repetitions | p50 (ms) | p95 (ms) | Status | Comparability caveat |","|---|---:|---:|---:|---|---|"]
    for p,q in c["C1"]["quantiles_ms"].items(): lines.append(f"| Teddy / {p} | {q['count']} | {fmt(q.get('p50'))} | {fmt(q.get('p95'))} | {c['C1']['status']} | Teddy C1 claim; comparator observations do not affect C1–C5. |")
    for r in result["comparators"].get("records",[]):
        if not r.get("attempts"): continue
        q=r.get("quantiles_ms",{}); name=r["name"]
        if name=="kak": caveat="Kakoune two-process/server model; editor runtime/config differs; non-comparable open-screen observation."
        elif name=="less": caveat="less is a demand-driven pager; editor runtime/config differs; non-comparable open-screen observation."
        else: caveat="Editor runtime/config differs from Teddy; non-comparable open-screen observation; does not affect C1–C5."
        lines.append(f"| {name} | {q.get('count',0)} | {fmt(q.get('p50'))} | {fmt(q.get('p95'))} | {r.get('status','-')} | {caveat} |")
    lines += ["","## Runtime claim details",f"- **C2:** {len(c['C2'].get('attempts',[]))} runtime attempts; {sum(a.get('line_count',0) for a in c['C2'].get('attempts',[]))} parsed perf lines; p95 `{c['C2'].get('quantiles_us',{}).get('p95')}` us; action association `{c['C2']['status'] == 'PASS'}`.",f"- **C3:** {len(c['C3'].get('attempts',[]))} PTY attempts; named prompt/search/cancel actions retained; semantic association `{c['C3']['status'] == 'PASS'}`.",f"- **C4:** {sum(1 for x in c['C4'].get('fixtures',[]) if x.get('status')=='PASS')}/{len(c['C4'].get('fixtures',[]))} fixture/profile digest checks passed.",f"- **C5:** {len(c['C5'].get('samples',[]))} workload sample records; RSS is diagnostic only and remains `NOT_MEASURED`.","","## Comparators","","| Tool | Status | Identity | Version probe |","|---|---|---|---|"]
    for t in result["comparators"]["tools"]:
        ident=f"`{t.get('path','-')}` `{t.get('sha256','-')[:12]}`" if t.get("path") else "-"; probe=t.get("version",{}).get("exit","-") if isinstance(t.get("version"),dict) else "-"; lines.append(f"| {t['name']} | {t['status']} | {ident} | exit `{probe}` |")
    lines += ["","## Comparator comparison","","| Tool | Status | Exact identity | Invocation class | Repetitions / p50 / p95 (ms) | Caveat |","|---|---|---|---|---|---|"]
    for r in result["comparators"].get("records",[]):
        q=r.get("quantiles_ms",{})
        fmt=lambda value: f"{value:.3f}" if isinstance(value,(int,float)) else "unavailable"
        metric=f"{q.get('count',0)} / {fmt(q.get('p50'))} / {fmt(q.get('p95'))}" if r.get("attempts") else "unavailable"
        name=r["name"]
        if r.get("status")=="ALIAS_OF": caveat="alias/rejected identity; not comparable"
        elif r.get("status") in {"UNAVAILABLE","UNSUPPORTED","REJECTED_UNSUPPORTED"}: caveat="unavailable/rejected: Kakoune uses a two-process server model; not comparable" if name=="kak" else "unavailable, unsupported, or rejected; not comparable"
        elif name=="kak": caveat="Kakoune two-process/server model; editor runtime/config differs; open-screen only, not a teddy claim"
        elif name=="less": caveat="less is a demand-driven pager; editor runtime/config differs; open-screen only, not a teddy claim"
        else: caveat="editor runtime/config differs from teddy; open-screen observation only, not a teddy claim"
        lines.append(f"| {name} | {r.get('status','-')} | `{r.get('path','unavailable')}` | `{r.get('invocation_class','-')}` | {metric} | {caveat} |")
    lines += ["","## Reproducibility","",f"- Command: `{result['command']}`",f"- Bundle: `{result['artifact_root']['path']}`",f"- Geometry: `{result['geometry'][0]}x{result['geometry'][1]}`; repetitions `{result['repetitions']}`; quiescence `{result['quiescence_ms']} ms`",f"- Environment: `{len(result['environment'])}` sanitized variables; host `{result['host']['os']}`, kernel `{result['host']['kernel']}`, arch `{result['host']['arch']}`", "- Evidence paths and SHA-256 values are recorded in the canonical JSON and remain immutable.","","## Limitations","","- PTY evidence measures application emission/transport and terminal-model screens, not physical rendering.","- C1 uses warm-cache runs without cache purge; identity/PGID probes are post-readiness and elapsed time is not complete process-launch latency.","- C2/C3 remain INCONCLUSIVE where action semantics cannot be established; C5 has no performance budget."]
    if len(lines)>250: raise ValueError("Phase 2 report exceeds reviewable line bound")
    return "\n".join(lines)+"\n"

def render_phase2_report(result):
    text=phase2_report_markdown(result); (REPO/"docs/bench_results.md").write_text(text); return text

def validate_phase2_report(result,text):
    if text!=phase2_report_markdown(result): raise ValueError("report does not equal pure canonical renderer")
    if len(text.splitlines())>250 or "```json" in text or '"profiles"' in text: raise ValueError("report is not concise Markdown")
    for k,v in result["claims"].items():
        if f"| {k} | **{v['status']}** |" not in text: raise ValueError("report claim parity failure")
    runtime=result["profiles"]["bare"]["files"]["teddy"]["version_capture"]["stdout"].strip()
    if result["artifact_root"]["path"] not in text or result["source"]["commit"] not in text or result["cargo_package_version"] not in text or runtime not in text: raise ValueError("report metadata parity failure")
    for t in result["comparators"]["tools"]:
        if f"| {t['name']} | {t['status']} |" not in text: raise ValueError("report comparator parity failure")
    for r in result["comparators"].get("records",[]):
        if f"| {r['name']} |" not in text: raise ValueError("report comparator table parity failure")
    for heading in ("## Executive summary","## C1 repetitions","## Runtime claim details","## Comparators","## Comparator comparison","## Reproducibility","## Limitations"):
        if heading not in text: raise ValueError("report required section missing")
    return True

def c1_attribution_plan():
    return [(block,mode,rep) for block,order in ((1,("current","deferred")),(2,("deferred","current"))) for rep in range(1,32) for mode in order]

def c1_attribution_criterion(current,deferred,removed_probe_ms):
    cp=phase2_quantiles(current)["p95"]; dp=phase2_quantiles(deferred)["p95"]; improvement=cp-dp if cp is not None and dp is not None else None
    paired=statistics.median([a-b for a,b in zip(current,deferred)]) if current and deferred else None
    agrees=removed_probe_ms>0 and paired is not None and abs(paired-removed_probe_ms)/removed_probe_ms<=.20
    return {"current_p95_ms":cp,"deferred_p95_ms":dp,"improvement_ms":improvement,"paired_reduction_ms":paired,"removed_probe_ms":removed_probe_ms,"criterion":bool(improvement is not None and improvement>=5 and agrees),"classification":"OBSERVER_CONTAMINATION" if improvement is not None and improvement>=5 and agrees else "NO_REPRODUCIBLE_OBSERVER_CONTAMINATION"}

def _c1_attribution_sample(binary,corpus,mode,root,rep,block=None,profile=None,helper=None):
    home=Path(tempfile.mkdtemp(prefix="s9-c1-attribution-")); argv=[str(binary),str(corpus)]; s=Session(argv,timeout=8,geometry=(200,50)); fork_wall=time.monotonic(); s.spawn(home,home,preflight=mode=="current"); parent_start_ms=(s.parent_start_wall-fork_wall)*1000
    identity_result=s.identity if mode=="current" else None; identity_ms=s.identity_probe_ms if mode=="current" else None; group_ok=group_rows=group_error=group_ms=None; group_started_ms=group_finished_ms=None
    if mode=="current":
        started=time.monotonic(); group_started_ms=(started-s.t0)*1000; group_ok,group_rows,group_error=process_group(s.original_pgid); group_finished_ms=(time.monotonic()-s.t0)*1000; group_ms=group_finished_ms-group_started_ms
    ready=s.until(lambda sc:sc.contains("S9_C1_ROW_000000"),"c1_attribution_ready");
    if mode=="deferred":
        started=time.monotonic(); identity_started_ms=(started-s.t0)*1000; identity_result=s._ps(); identity_finished_ms=(time.monotonic()-s.t0)*1000; identity_ms=identity_finished_ms-identity_started_ms; started=time.monotonic(); group_started_ms=(started-s.t0)*1000; group_ok,group_rows,group_error=process_group(s.original_pgid); group_finished_ms=(time.monotonic()-s.t0)*1000; group_ms=group_finished_ms-group_started_ms; s.identity=identity_result
    else: identity_started_ms=s.identity_probe_started_ms; identity_finished_ms=s.identity_probe_finished_ms
    readiness={"matched":ready.get("matched"),"name":ready.get("name"),"matched_at":ready.get("matched_at"),"snapshot":ready.get("snapshot")}
    expected_root=[argv[0],argv[1]]; expected_helper=[helper] if helper else None; group_identity=exact_group_identity((group_ok,group_rows,group_error),expected_root,expected_helper)
    probes={"identity":{"result":identity_result,"verified":bool(identity_result and identity_result.get("verified")),"error":identity_result.get("error") if isinstance(identity_result,dict) else "probe did not run","duration_ms":identity_ms,"started_ms":identity_started_ms,"finished_ms":identity_finished_ms},"process_group":{"success":group_ok,"rows":group_rows,"error":group_error,"duration_ms":group_ms,"started_ms":group_started_ms,"finished_ms":group_finished_ms,"identity":group_identity}}
    clean=s.close(); process=s.record(); identity_ok=bool(identity_result and identity_result.get("verified") and identity_result.get("argv")==argv); probe_ok=bool(probes["identity"]["verified"] and probes["identity"]["error"] is None and group_identity[0]); lifecycle=clean and process.get("exit")==0 and process.get("signal") is None and process.get("reaped") and process.get("pty_eof") and process.get("stderr_eof") and process.get("drain_complete") and not process.get("drain_deadline") and not process.get("timed_out") and not process.get("pty_output_capped") and not process.get("stderr_capped") and not process.get("unsupported") and not process.get("exec_failed") and not process.get("cleanup_error") and not process.get("pgid_after") and not process.get("descendants_left") and not process.get("pgid_probe_error") and not process.get("pgid_before_probe_error") and not process.get("pgid_after_probe_error"); valid=bool(ready.get("matched") and identity_ok and probe_ok and lifecycle and readiness["snapshot"] and any("S9_C1_ROW_000000" in line for line in readiness["snapshot"]))
    reason="valid paired attribution sample" if valid else "invalid readiness/identity/probe/lifecycle evidence"
    return {"block":block,"profile":profile,"mode":mode,"rep":rep,"status":"PASS" if valid else "INCONCLUSIVE","valid":valid,"validity_reason":reason,"argv":argv,"parent_start_ms":parent_start_ms,"probes":probes,"first_output_ms":s.first_output*1000 if s.first_output is not None else None,"readiness_ms":readiness["matched_at"]*1000 if readiness["matched_at"] is not None else None,"readiness":readiness,"identity":s.identity,"group_identity":group_identity,"process":process}

def _c1_attribution_samples_by_rep(samples,profile,mode):
    selected=[s for s in samples if s.get("profile")==profile and s.get("mode")==mode]
    keys=[(s.get("block"),s.get("rep")) for s in selected]; expected={(b,r) for b in (1,2) for r in range(1,32)}
    if len(selected)!=62 or set(keys)!=expected or len(set(keys))!=62: raise ValueError("C1 attribution block/rep matrix mismatch")
    return {(s["block"],s["rep"]):s for s in selected}

def _c1_attribution_summary(samples,profile):
    current=_c1_attribution_samples_by_rep(samples,profile,"current"); deferred=_c1_attribution_samples_by_rep(samples,profile,"deferred")
    invalid=[s for s in list(current.values())+list(deferred.values()) if s.get("status")!="PASS" or not s.get("valid")]
    if invalid:
        bad={"current_p95_ms":None,"deferred_p95_ms":None,"improvement_ms":None,"paired_reduction_ms":None,"removed_probe_ms":None,"criterion":False,"classification":"NO_REPRODUCIBLE_OBSERVER_CONTAMINATION","valid":False,"validity_reason":"invalid required sample: "+invalid[0].get("validity_reason","unspecified")}
        return {"blocks":{"1":bad,"2":bad},"aggregate":bad}
    blocks={}
    for block in (1,2):
        keys=[(block,r) for r in range(1,32)]; cv=[current[k]["readiness_ms"] for k in keys]; dv=[deferred[k]["readiness_ms"] for k in keys]; removed=statistics.median([current[k]["probes"]["identity"]["duration_ms"]+current[k]["probes"]["process_group"]["duration_ms"] for k in keys]); paired=statistics.median([a-b for a,b in zip(cv,dv)]); result=c1_attribution_criterion(cv,dv,removed); result.update({"valid":True,"validity_reason":"all required paired block/rep samples valid","paired_reduction_ms":paired,"removed_probe_ms":removed}); blocks[str(block)]=result
    keys=sorted(current); cv=[current[k]["readiness_ms"] for k in keys]; dv=[deferred[k]["readiness_ms"] for k in keys]; removed=statistics.median([current[k]["probes"]["identity"]["duration_ms"]+current[k]["probes"]["process_group"]["duration_ms"] for k in keys]); paired=statistics.median([a-b for a,b in zip(cv,dv)]); aggregate=c1_attribution_criterion(cv,dv,removed); aggregate.update({"valid":True,"validity_reason":"all required paired block/rep samples valid","paired_reduction_ms":paired,"removed_probe_ms":removed}); return {"blocks":blocks,"aggregate":aggregate}

def validate_c1_attribution_report(report):
    if not isinstance(report,dict) or report.get("schema")!="teddy-s9-c1-attribution-1" or report.get("diagnostic")!="c1-attribution": raise ValueError("invalid C1 attribution schema")
    root=report.get("artifact_root",{}).get("path"); p=Path(root) if isinstance(root,str) else None
    if p is None or p.is_absolute() or ".." in p.parts or not root.startswith("bench/artifacts/"): raise ValueError("unsafe attribution artifact root")
    if report.get("geometry")!=[200,50] or report.get("corpus",{}).get("size")!=1<<30 or not report.get("corpus",{}).get("path"): raise ValueError("invalid attribution geometry/corpus")
    root_path=Path(root); source=report.get("source",{}); if_source=source.get("commit") if isinstance(source,dict) else None
    if not isinstance(if_source,str) or not isinstance(source.get("dirty"),bool): raise ValueError("missing attribution provenance")
    corpus=report["corpus"]; corpus_path=_artifact_path({"path":corpus["path"],"sha256":corpus.get("sha256")}, {"path":root})
    if not corpus_path.is_file() or sha(corpus_path)[0]!=corpus.get("sha256") or corpus_path.stat().st_size!=1<<30: raise ValueError("invalid attribution corpus provenance")
    provenance=report.get("profile_provenance")
    if not isinstance(provenance,dict) or set(provenance)!={"bare","shipped"}: raise ValueError("missing staged profile provenance")
    for profile,info in provenance.items():
        expected_files={"teddy"} if profile=="bare" else {"teddy","teddy-highlight"}
        if set(info.get("files",{}))!=expected_files or set(info.get("exact_files",[]))!=expected_files: raise ValueError("invalid staged profile file matrix")
        for name,meta in info["files"].items():
            fp=Path(meta.get("path",""));
            try: fp.resolve().relative_to(root_path.resolve())
            except ValueError: raise ValueError("staged profile escapes artifact root")
            if not fp.is_file() or sha(fp)[0]!=meta.get("sha256") or fp.stat().st_size!=meta.get("size") or meta.get("path")!=str(fp): raise ValueError("staged profile provenance mismatch")
    expected_plan=[list(x) for x in c1_attribution_plan()]
    if report.get("warmups")!=5 or report.get("plan")!=expected_plan: raise ValueError("invalid attribution plan")
    samples=report.get("samples"); profiles=report.get("profiles")
    if not isinstance(samples,list) or not isinstance(profiles,dict) or set(profiles)!={"bare","shipped"} or len(samples)!=248: raise ValueError("invalid attribution sample matrix")
    for s in samples:
        if s.get("block") not in {1,2} or s.get("rep") not in range(1,32) or s.get("mode") not in {"current","deferred"} or s.get("profile") not in profiles: raise ValueError("invalid attribution sample identity")
        info=provenance[s["profile"]]; expected_binary=info["files"]["teddy"]["path"]; expected_corpus=str((REPO/corpus["path"]).resolve()) if corpus["path"].startswith("bench/") else str((root_path/corpus["path"]).resolve()); expected_argv=[expected_binary,expected_corpus]
        if s.get("argv")!=expected_argv or s.get("identity",{}).get("argv")!=expected_argv: raise ValueError("attribution sample provenance mismatch")
        if not isinstance(s.get("probes"),dict) or not isinstance(s["probes"].get("identity"),dict) or not isinstance(s["probes"].get("process_group"),dict): raise ValueError("missing attribution probe evidence")
        i,g=s["probes"]["identity"],s["probes"]["process_group"]; pr=s.get("process",{}); identity=i.get("result",{})
        if not isinstance(identity,dict) or i.get("verified") is not (identity.get("verified") is True) or not isinstance(i.get("duration_ms"),(int,float)) or i.get("error") is not None or g.get("success") is not True or g.get("error") is not None or not isinstance(g.get("rows"),list) or not isinstance(g.get("duration_ms"),(int,float)): raise ValueError("invalid attribution probe evidence")
        if s.get("identity")!=identity or identity.get("argv")!=s.get("argv") or not isinstance(s.get("argv"),list) or len(s["argv"])!=2: raise ValueError("attribution identity argv mismatch")
        readiness=s.get("readiness")
        if not isinstance(readiness,dict) or readiness.get("matched") is not True or readiness.get("name")!="c1_attribution_ready" or not isinstance(readiness.get("snapshot"),list) or not any("S9_C1_ROW_000000" in line for line in readiness["snapshot"] if isinstance(line,str)) or readiness.get("matched_at")*1000!=s.get("readiness_ms"): raise ValueError("invalid attribution readiness evidence")
        if not isinstance(s.get("first_output_ms"),(int,float)) or s["first_output_ms"]>s["readiness_ms"]: raise ValueError("invalid attribution output ordering")
        for key in ("identity","process_group"):
            probe=s["probes"][key];
            if not isinstance(probe.get("started_ms"),(int,float)) or not isinstance(probe.get("finished_ms"),(int,float)) or probe["finished_ms"]<probe["started_ms"]: raise ValueError("invalid attribution probe timing")
            if s["mode"]=="current" and probe["finished_ms"]>s["first_output_ms"]: raise ValueError("current probe did not precede PTY drain")
            if s["mode"]=="deferred" and probe["started_ms"]<s["readiness_ms"]: raise ValueError("deferred probe preceded readiness")
        expected_helper=info.get("helper") if s["profile"]=="shipped" else None; expected_group=expected_argv; group=s["probes"]["process_group"]; derived_group=exact_group_identity((group.get("success"),group.get("rows"),group.get("error")),expected_group,[expected_helper] if expected_helper else None)
        stored_group=tuple(s.get("group_identity")) if isinstance(s.get("group_identity"),list) else s.get("group_identity")
        if not isinstance(stored_group,tuple) or len(stored_group)!=2 or stored_group[0] is not derived_group[0] or stored_group[1]!=derived_group[1] or derived_group[0] is not True: raise ValueError("attribution process group identity mismatch")
        if s.get("status") not in {"PASS","INCONCLUSIVE"}: raise ValueError("invalid attribution sample status")
        valid=bool(s.get("status")=="PASS" and s.get("valid") is True and s.get("readiness_ms") is not None and identity.get("verified") is True and pr.get("exit")==0 and pr.get("signal") is None and pr.get("reaped") and pr.get("pty_eof") and pr.get("stderr_eof") and pr.get("drain_complete") and not pr.get("drain_deadline") and not pr.get("timed_out") and not pr.get("pty_output_capped") and not pr.get("stderr_capped") and not pr.get("unsupported") and not pr.get("exec_failed") and not pr.get("cleanup_error") and not pr.get("pgid_after") and not pr.get("descendants_left") and not pr.get("pgid_probe_error") and not pr.get("pgid_before_probe_error") and not pr.get("pgid_after_probe_error"));
        if valid is not (s.get("status")=="PASS"): raise ValueError("attribution status/lifecycle mismatch")
    for profile in profiles:
        derived=_c1_attribution_summary(samples,profile)
        if profiles[profile]!=derived: raise ValueError("attribution profile arithmetic/classification mismatch")
    expected="OBSERVER_CONTAMINATION" if all(v.get("aggregate",{}).get("classification")=="OBSERVER_CONTAMINATION" and all(b.get("classification")=="OBSERVER_CONTAMINATION" for b in v.get("blocks",{}).values()) for v in profiles.values()) else "NO_REPRODUCIBLE_OBSERVER_CONTAMINATION"
    if report.get("classification")!=expected: raise ValueError("attribution top-level classification mismatch")
    return True

def c1_attribution(a):
    if not a.allow_large: raise ValueError("C1 attribution requires --allow-large")
    run=ROOT/"artifacts"/("c1-attribution-"+time.strftime("%Y%m%dT%H%M%SZ",time.gmtime())+"-"+uuid.uuid4().hex[:10]); run.mkdir(parents=True); build=run/"build-target"; subprocess.check_call(["cargo","build","--release","--target-dir",str(build)],cwd=REPO); built=build/"release"; profiles=_phase2_profiles(run/"profiles",built); corpus=run/"corpus"/"s9-1g.log"; generate_phase2_log(corpus); warmups=5; samples=[]; plan=c1_attribution_plan()
    for profile,p in profiles.items():
        for mode in ("current","deferred"):
            for i in range(1,warmups+1): _c1_attribution_sample(p["root"],corpus,mode,run,i)
        for block,mode,i in plan: samples.append(_c1_attribution_sample(p["root"],corpus,mode,run,i,block,profile,p.get("helper")))
    summary={profile:_c1_attribution_summary(samples,profile) for profile in profiles}
    source={"commit":subprocess.check_output(["git","rev-parse","HEAD"],cwd=REPO,text=True).strip(),"dirty":bool(subprocess.check_output(["git","status","--porcelain"],cwd=REPO))}; profile_provenance={p:{"root":v["root"],"helper":v["helper"],"files":v["files"],"exact_files":v["exact_files"]} for p,v in profiles.items()}; corpus_meta={"path":str(corpus.relative_to(REPO)),"size":corpus.stat().st_size,"sha256":sha(corpus)[0]}; report={"schema":"teddy-s9-c1-attribution-1","diagnostic":"c1-attribution","artifact_root":{"path":str(run.relative_to(REPO))},"geometry":[200,50],"source":source,"profile_provenance":profile_provenance,"corpus":corpus_meta,"warmups":warmups,"plan":[list(x) for x in plan],"profiles":summary,"samples":samples,"classification":"OBSERVER_CONTAMINATION" if all(v["aggregate"]["classification"]=="OBSERVER_CONTAMINATION" and all(b["classification"]=="OBSERVER_CONTAMINATION" for b in v["blocks"].values()) for v in summary.values()) else "NO_REPRODUCIBLE_OBSERVER_CONTAMINATION","claim":"methodology attribution only; no editor performance claim"}; validate_c1_attribution_report(report); (run/"report.json").write_text(json.dumps(report,indent=2,sort_keys=True)+"\n"); print(json.dumps(report,indent=2,sort_keys=True))

def full(a):
    if not a.allow_large: raise ValueError("Phase 2 requires --allow-large")
    run=ROOT/"artifacts"/("phase2-"+time.strftime("%Y%m%dT%H%M%SZ",time.gmtime())+"-"+uuid.uuid4().hex[:10]); run.mkdir(parents=True); (run/"c1").mkdir(); label=str(run.relative_to(REPO)); build=run/"build-target"; subprocess.check_call(["cargo","build","--release","--target-dir",str(build)],cwd=REPO); built=build/"release"; profiles=_phase2_profiles(run/"profiles",built); large=run/"corpus"/"s9-1g.log"; manifest=generate_phase2_log(large); large_meta=_phase2_file(large,REPO); reps={p:[_phase2_rep(v,large,run/"c1",run.name,i) for i in range(1,6)] for p,v in profiles.items()}; c1q={p:phase2_quantiles([r["elapsed_ms"] for r in rs if r["elapsed_ms"] is not None]) for p,rs in reps.items()}; c1_valid=all(all(r["status"]=="PASS" for r in rs) for rs in reps.values()); c1status="PASS" if c1_valid and all(c1q[p]["p95"]<50 for p in reps) else ("FAIL" if c1_valid else "INCONCLUSIVE")
    perf_target=run/"perf-target"; subprocess.check_call(["cargo","build","--release","--features","perf","--target-dir",str(perf_target)],cwd=REPO); perf_rows=[]
    perf_profiles=_phase2_profiles(run/"perf-profiles",perf_target/"release"); c2_attempts=[]; c2_evidence=[]
    for p in perf_profiles.values():
        attempt,ev=_perf_attempt(p,run); c2_attempts.append(attempt); c2_evidence.append(ev); c2_evidence.append(attempt["log"])
    c2_rows=[row for attempt in c2_attempts for row in attempt["parsed_rows"]]; c2_status="PASS" if c2_rows and all(a["ready"] and a["action"] and a["clean"] and a["association"]=="action" for a in c2_attempts) and phase2_quantiles([r["us"] for r in c2_rows])["p95"]<1000 else "INCONCLUSIVE"
    c4=[]; c4_evidence=[]
    for p in profiles:
        for n in ("edit-crlf.txt","edit-invalid.txt"):
            rr=scenario(p,run/"profiles",ROOT/"corpus",run/"c4",n,4); source_unchanged=hashlib.sha256((ROOT/"corpus"/n).read_bytes()).hexdigest()==FIX[n]["sha256"]; saved=rr.get("evidence",{}).get("saved_output",{}); actual_saved= saved.get("sha256") if saved else None; expected=FIX[n]["post_edit_sha256"]; c4.append({"profile":p,"fixture":n,"status":"PASS" if rr["status"]=="PASS" and source_unchanged and actual_saved==expected else "FAIL","expected":expected,"actual_saved_sha256":actual_saved,"saved_output":saved,"source_unchanged":source_unchanged}); c4_evidence.extend(rr.get("evidence",{}).values())
    c3_attempts=[]; c3_evidence=[]
    for p in profiles.values():
        attempt,ev=_c3_attempt(p,large,run); c3_attempts.append(attempt); c3_evidence.append(ev)
    rss_samples=[{"profile":p,"rep":r["rep"],"samples":r["rss_samples"],"peak_bytes":r["rss_peak_bytes"]} for p,rs in reps.items() for r in rs]; rss_path=run/"c5-rss.json"; rss_path.write_text(json.dumps({"samples":rss_samples,"status":"NOT_MEASURED","reason":"RSS samples are diagnostic only"},sort_keys=True)+"\n"); rss_evidence=_phase2_file(rss_path,REPO)
    comparators=discover_comparators(); comparator_records,comparator_evidence=run_comparators(comparators,large,run); evidence=[large_meta]+[r["artifact"] for rs in reps.values() for r in rs]+c2_evidence+c3_evidence+[rss_evidence]+c4_evidence+comparator_evidence
    result={"schema":PHASE2_SCHEMA,"phase":"phase2","methodology":PHASE2_METHODOLOGY,"historical_context":PHASE2_HISTORICAL_CONTEXT,"command":"python3 bench/bench.py full --allow-large","artifact_root":{"path":label,"resolvable_from":"repository root"},"source":{"commit":subprocess.check_output(["git","rev-parse","HEAD"],cwd=REPO,text=True).strip(),"dirty":bool(subprocess.check_output(["git","status","--porcelain"],cwd=REPO))},"cargo_package_version":json.loads(subprocess.check_output(["cargo","metadata","--no-deps","--format-version","1"],cwd=REPO,text=True))["packages"][0]["version"],"profiles":{p:{"files":v["files"],"exact_files":v["exact_files"]} for p,v in profiles.items()},"environment":phase2_environment(),"host":{"os":platform.platform(),"kernel":platform.release(),"arch":platform.machine(),"cpu":os.cpu_count(),"free_bytes":shutil.disk_usage(REPO).free},"geometry":[200,50],"repetitions":5,"quiescence_ms":5,"corpus":{"manifest":manifest,"evidence":large_meta},"claims":{"C1":{"methodology":PHASE2_METHODOLOGY,"status":c1status,"reason":"validated teddy-only warm PTY repetitions; predeclared p95 threshold is 50ms","profiles":reps,"quantiles_ms":c1q},"C2":{"status":c2_status,"reason":"runtime perf PTY attempt; action association is required for PASS","attempts":c2_attempts,"quantiles_us":phase2_quantiles([r["us"] for r in c2_rows])},"C3":{"status":"INCONCLUSIVE","reason":"real literal-search/cancellation PTY attempts retained; UI action association is not established","attempts":c3_attempts,"evidence":c3_evidence},"C4":{"status":"PASS" if all(x["status"]=="PASS" for x in c4) else "FAIL","reason":"isolated teddy integration and declared fixture digest verification","fixtures":c4},"C5":{"status":"NOT_MEASURED","reason":"RSS samples are diagnostic only; no performance budget","samples":rss_samples,"evidence":rss_evidence}},"comparators":{"status":"DISCOVERED","tools":comparators,"records":comparator_records},"evidence":evidence,"limitations":["PTY records application emission/transport, not physical rendering","warm cache/no purge","rendered screens are terminal-model evidence","renderer diagnostic remains a known observed failure"]}; validate_phase2_result(result); (REPO/"bench/results").mkdir(parents=True,exist_ok=True); (REPO/"bench/results/canonical.json").write_text(json.dumps(result,indent=2,sort_keys=True)+"\n"); validate_phase2_result(result); render_phase2_report(result); validate_phase2_report(result,(REPO/"docs/bench_results.md").read_text()); print(json.dumps(result,indent=2,sort_keys=True))

def helper(a):
    if a.mode=="ansi": print("\033[?25l\033[?1049h\033[2J\033[1;1Hraw helper\033[2K\033[?1049l\033[?25h")
    elif a.mode=="sleep": time.sleep(a.seconds)
    else: print("helper")
def tests():
    if not unittest.TextTestRunner(verbosity=1).run(unittest.defaultTestLoader.discover(str(ROOT/"tests"))).wasSuccessful(): raise SystemExit(1)
def main(argv=None):
    p=argparse.ArgumentParser(); s=p.add_subparsers(dest="cmd",required=True); g=s.add_parser("generate");g.add_argument("-o","--output",default=str(ROOT/"corpus"));g.set_defaults(fn=lambda a:print(json.dumps(generate(a.output),indent=2,sort_keys=True))); q=s.add_parser("smoke");q.add_argument("--corpus",default=str(ROOT/"corpus"));q.add_argument("--timeout",type=float,default=4);q.add_argument("-o","--output");q.add_argument("--artifact-root");q.set_defaults(fn=smoke); f=s.add_parser("full");f.add_argument("--allow-large",action="store_true");f.set_defaults(fn=full); d=s.add_parser("c1-attribution");d.add_argument("--allow-large",action="store_true");d.set_defaults(fn=c1_attribution); r=s.add_parser("report");r.add_argument("input");r.add_argument("-o","--output");r.set_defaults(fn=report); t=s.add_parser("test");t.set_defaults(fn=lambda a:tests()); h=s.add_parser("helper");h.add_argument("mode",choices=("ansi","sleep","echo"),nargs="?",default="echo");h.add_argument("--seconds",type=float,default=.1);h.set_defaults(fn=helper); a=p.parse_args(argv); a.fn(a)
if __name__=="__main__": main()
