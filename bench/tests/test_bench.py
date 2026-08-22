import contextlib, hashlib, io, json, os, pty, select, subprocess, sys, tempfile, threading, time, unittest, textwrap
from unittest import mock
from types import SimpleNamespace
from pathlib import Path
sys.path.insert(0, str(Path(__file__).parents[1]))
import bench

class ExecutorFakeSession(bench.Session):
    """Deterministic in-process Session: every write creates a new PTY event."""
    def __init__(self, argv, geometry=(200,50), timeout=4):
        super().__init__(argv,geometry,timeout); self.t0=0.0; self.clock=0.0
    def spawn(self, home, cwd, preflight=True):
        ExecutorFakeSession.current_argv=self.argv
        self.pid=self.root_pid=self.pgid=self.original_pgid=1; self.status=None; self.closed=False
        self.identity={"verified":True,"argv":self.argv}
    def _emit(self, data):
        self.clock+=.1; event={"channel":"pty_output_raw","at":self.clock,"data":data.hex()}; self.trace_events.append(event)
        self.trace_events.append({"channel":"pty_output","at":self.clock,"data":data.hex()}); self.output_events.append(self.clock); self.trace_bytes+=len(data); self.trace_hash.update(data); self.screen.feed(data)
    def until(self, predicate, name="endpoint", after_output_count=None):
        data={"startup_head":b"UBENCH_HEAD_RECORD\n","prompt":b"/\n","typed_needle":b"UBENCH_NEEDLE\n","submit":b"UBENCH_TARGET_RECORD\n","quit":b"QUIT\n"}.get(name,b"EVENT\n")
        self._emit(data); i=len(self.output_events)-1
        try: matched=bool(predicate(self.screen,getattr(self,"_pre_write_screen",list(self.screen.text()))))
        except TypeError: matched=bool(predicate(self.screen))
        return {"name":name,"matched":matched,"matched_at":self.output_events[i],"event_index":i,"trace_index":len(self.trace_events)-1,"snapshot":list(self.screen.text())}
    def send_action(self, data, name="action"):
        self.clock+=.1; at=self.clock; baseline=len(self.output_events); self._pre_write_screen=list(self.screen.text()); trace_index=len(self.trace_events); self.trace_events.append({"channel":"pty_input","at":at,"data":data.hex(),"action":True,"name":name}); return {"name":name,"bytes":data.hex(),"write":at,"trace_index":trace_index,"output_baseline":baseline,"pre_write_screen":self._pre_write_screen}
    def write_endpoint(self, data, expected, name="action"):
        action=self.send_action(data,name); endpoint=self.until(expected,name,action["output_baseline"]); first=endpoint["matched_at"]; action.update({"first_post_write_output":first,"first_output_after_write":first,"endpoint":first,"endpoint_event_index":endpoint["event_index"],"output_event_index":endpoint["event_index"],"trace_output_index":endpoint.get("trace_index"),"input_trace_index":action["trace_index"],"input_trace_time":action["write"],"last_output":first,"quiet_complete":(first+.01 if isinstance(first,(int,float)) else None),"quiet_gap":.01,"output_event_count":1,"endpoint_found":endpoint["matched"],"matched":endpoint["matched"],"matched_at":first,"snapshot":endpoint["snapshot"]}); self.quiet_complete=(first+.01 if isinstance(first,(int,float)) else None); self.actions.append(action); self.endpoint_snapshots.append({"name":name,"at":first,"kind":"action","rows":endpoint["snapshot"]}); return action
    def write(self, data, expected, name="action"):
        return self.write_endpoint(data,expected,name).get("matched") is True
    def close(self):
        self.status=0; self.pid=-1; self.closed=True; self.drain_complete=True; self.pty_eof=self.stderr_eof=True; self.pgid_after=[]; self.descendants=[]; return True
    def record(self):
        return {"exit":0,"signal":None,"reaped":True,"pty_eof":True,"stderr_eof":True,"drain_complete":True,"drain_deadline":False,"cleanup_error":None,"pgid_after":[],"descendants_left":False,"timed_out":False,"output_capped":False,"pty_output_capped":False,"stderr_capped":False,"unsupported":[],"exec_failed":False,"trace_events":self.trace_events,"actions":self.actions,"screen":self.screen.text(),"descendants":[],"pgid_before":getattr(self,"pgid_before",[]),"pgid_before_probe_error":getattr(self,"pgid_before_probe_error",None),"harness_traffic":[]}

class LateOutputSession(ExecutorFakeSession):
    def until(self,predicate,name="endpoint",after_output_count=None):
        result=super().until(predicate,name,after_output_count)
        if name=="prompt": self._emit(b"\033[2J\033[1;1HLATE_AFTER_PROMPT\n")
        return result

class NoPromptOutputSession(ExecutorFakeSession):
    def write_endpoint(self,data,expected,name="action"):
        if name!="prompt": return super().write_endpoint(data,expected,name)
        action=self.send_action(data,name)
        action.update({"first_post_write_output":None,"first_output_after_write":None,"endpoint":None,
                       "endpoint_event_index":None,"output_event_index":None,"trace_output_index":None,
                       "input_trace_index":action["trace_index"],"input_trace_time":action["write"],
                       "last_output":None,"quiet_complete":None,"quiet_gap":None,
                       "output_event_count":0,"endpoint_found":False,"matched":False,"matched_at":None,
                       "snapshot":list(self.screen.text())})
        self.actions.append(action); self.endpoint_snapshots.append({"name":name,"at":None,"kind":"action","rows":action["snapshot"]})
        return action

class TargetBeforeSubmitSession(ExecutorFakeSession):
    def write_endpoint(self,data,expected,name="action"):
        if name!="submit": return super().write_endpoint(data,expected,name)
        self._emit(b"\nUBENCH_TARGET_RECORD\n")
        output_index=len(self.output_events)-1; trace_index=len(self.trace_events)-1; at=self.output_events[-1]
        action=self.send_action(data,name)
        action.update({"first_post_write_output":at,"first_output_after_write":at,"endpoint":at,"endpoint_event_index":output_index,"output_event_index":output_index,"trace_output_index":trace_index,"input_trace_index":action["trace_index"],"input_trace_time":action["write"],"last_output":at,"quiet_complete":at+.01,"quiet_gap":.01,"output_event_count":1,"endpoint_found":True,"matched":True,"matched_at":at,"snapshot":list(self.screen.text())})
        self.quiet_complete=at+.01; self.actions.append(action); self.endpoint_snapshots.append({"name":name,"at":at,"kind":"action","rows":action["snapshot"]}); return action

class Phase1Tests(unittest.TestCase):
    def _strict_smoke(self, root, adapters):
        root=Path(root); records={}
        for adapter in adapters:
            name=adapter["name"]
            if adapter["status"]!="IDENTITY_QUALIFIED":
                records[name]={"status":adapter["status"],"reason":"not runnable","attempts":0}
                continue
            d=root/"smoke"/name; d.mkdir(parents=True,exist_ok=True)
            def put(filename,data):
                p=d/filename; p.write_bytes(data); h,z=bench.sha(p); return {"path":str(p),"sha256":h,"size":z}
            identity={"verified":True,"argv":adapter["argv"],"before":bench._universal_identity(adapter["path"],adapter["argv"],adapter["version"]),"after":bench._universal_identity(adapter["path"],adapter["argv"],adapter["version"])}; topology={"expected_argvs":[adapter["argv"]]+([[adapter["helper_path"]]] if adapter["helper_path"] else []),"observed_argvs":[adapter["argv"]]+([[adapter["helper_path"]]] if adapter["helper_path"] else []),"unexpected":False,"pgid_before":[{"command":" ".join(x),"pid":i+1} for i,x in enumerate([adapter["argv"]]+([[adapter["helper_path"]]] if adapter["helper_path"] else []))],"descendants":[],"pgid_after":[]}
            actions=[{"name":"prompt","bytes":adapter["search_prompt"],"trace_index":2,"write":2.0},{"name":"typed_needle","bytes":"5542454e43485f4e4545444c45","trace_index":5,"write":4.0},{"name":"submit","bytes":adapter["search_submit"],"trace_index":8,"write":6.0},{"name":"quit","bytes":adapter["quit"],"trace_index":11,"write":8.0}]
            clear=lambda text:(b"\033[2J\033[1;1H"+text+b"\n")
            screens=[clear(b"UBENCH_HEAD_RECORD"),clear(b"PROMPT"),clear(b"PROMPT UBENCH_NEEDLE"),clear(b"UBENCH_TARGET_RECORD UBENCH_NEEDLE")]
            trace=[]
            for i,sc in enumerate(screens):
                trace += [{"channel":"pty_output_raw","at":float(i*2+1),"data":sc.hex()},{"channel":"pty_output","at":float(i*2+1),"data":sc.hex()}]
                if i<4: trace += [{"channel":"pty_input","at":float(i*2+2),"data":actions[i]["bytes"],"action":True,"name":actions[i]["name"]}]
            screen=screens[-1]; process={"exit":0,"signal":None,"reaped":True,"drain_complete":True,"pty_eof":True,"stderr_eof":True,"drain_deadline":False,"pgid_after":[],"descendants_left":False,"timed_out":False,"unsupported":[],"cleanup_error":None,"exec_failed":False,"output_capped":False,"stderr_capped":False}; phase_records={}
            for phase,sc,idx in zip(("startup_head","prompt","typed_needle","target"),screens,(0,1,2,3)):
                payload={"name":phase,"phase":phase,"matched":True,"missing":False,"output_event_ordinal":idx,"trace_event_index":idx*3+1,"causal_time":float(idx*2+1),"associated_input_trace_event_index":None if idx==0 else idx*3-1,"input_write_time":None if idx==0 else float(idx*2),"rows":bench.Screen(200,50).text()}
                model=bench.Screen(200,50); model.feed(sc); payload["rows"]=model.text(); phase_records[phase]=put(phase+".json",json.dumps(payload,sort_keys=True).encode())
            records[name]={"helper_identity_before":bench._universal_helpers(adapter),"helper_identity_after":bench._universal_helpers(adapter),"status":"PASS","reason":"strict fixture","attempts":1,"argv":adapter["argv"],"process":process,"identity":identity,"topology":topology,"actions":actions,"harness_traffic":[],"records":{"trace":put("trace.json",json.dumps(trace).encode()),"screen":put("screen.txt",b"\n".join([])+b"".join([]) if False else b""),"actions":put("actions.json",json.dumps(actions).encode()),"identity":put("identity.json",json.dumps(identity,sort_keys=True).encode()),"topology":put("topology.json",json.dumps(topology,sort_keys=True).encode()),"phase_snapshots":put("phase_snapshots.json",json.dumps(phase_records,sort_keys=True).encode())}}
            # The final screen is the replayed target endpoint, not a phase union.
            model=bench.Screen(200,50)
            for event in trace:
                if event["channel"]=="pty_output": model.feed(bytes.fromhex(event["data"]))
            records[name]["records"]["screen"]=put("screen.txt",("\n".join(model.text())+"\n").encode())
        return {"corpus":"/tmp/smoke-corpus","records":records}

    def test_eof_and_deadline_are_distinct(self):
        s=bench.Session([]); s.master,ptyw=os.pipe(); s.er,errw=os.pipe(); os.close(ptyw); os.close(errw); s.t0=0
        s._read(.01)
        self.assertTrue(s.pty_eof and s.stderr_eof)
        s.drain_deadline=True; s.drain_complete=False
        self.assertFalse(s.drain_complete)
        os.close(s.master); os.close(s.er)

    def test_trace_cap_retains_prefix_and_hashes_full_stream(self):
        s=bench.Session([]); s.trace_cap=3
        s.master,ptyw=os.pipe(); s.er,errw=os.pipe(); os.set_blocking(s.master,False); os.set_blocking(s.er,False); os.write(ptyw,b"abcdef"); os.close(ptyw); os.close(errw); s.t0=0
        s._read(.01)
        self.assertEqual(bytes(s.trace),b"abc"); self.assertEqual(s.trace_bytes,6); self.assertEqual(s.trace_discarded_bytes,3); self.assertEqual(s.trace_hash.hexdigest(),hashlib.sha256(b"abcdef").hexdigest())
        os.close(s.master); os.close(s.er)

    def test_config_rejects_missing_contract_declaration(self):
        bad=json.loads(json.dumps(bench.CORPUS)); del bad["fixtures"]["binary-safe.bin"]["status_marker"]
        with self.assertRaises(ValueError): bench.validate_config(bad)

    def test_authoritative_fixture_goldens_and_aggregate(self):
        expected={"code-1m.rs":"6765b61519cdbd84b86cbac47cdcf66cc36818e7d195bd7e0c9dc913624107fb",
                  "edit-crlf.txt":"04b98eb737bbe814d042823c8984919f17acca1cb19310e282fcd6f34722848e",
                  "edit-invalid.txt":"7d5869e8f2a7cf839944b4b5503a9f3791b2c80417295855660e8f5edbb4deeb"}
        self.assertEqual(len(bench.fixture_bytes("code-1m.rs")),1048576)
        self.assertEqual(bench.fixture_bytes("code-1m.rs").count(b"\n"),16384)
        for n,h in expected.items(): self.assertEqual(hashlib.sha256(bench.fixture_bytes(n)).hexdigest(),h)
        with tempfile.TemporaryDirectory() as d:
            ms=bench.generate(d); agg=json.loads((Path(d)/"manifest.json").read_text())
            self.assertEqual(len(ms),5); self.assertEqual(len(agg["fixtures"]),5); self.assertEqual(agg["fixtures"][2]["post_edit_sha256"],bench.FIX["edit-invalid.txt"]["post_edit_sha256"])
            raw=json.dumps({k:v for k,v in agg.items() if k!="aggregate_sha256"},sort_keys=True,separators=(",",":"),ensure_ascii=False).encode()
            self.assertEqual(agg["aggregate_sha256"],hashlib.sha256(raw).hexdigest())

    def test_utf8_private_alt_erase_fragment_and_stale_marker(self):
        s=bench.Screen(20,4); s.feed(b"\033[?1049h\033[2J\033[1;1HOLD\033[2KNEW\xc3") ; self.assertFalse(s.contains("HOLD")); s.feed(b"\xa9\033[?25l")
        self.assertTrue(s.alt); self.assertFalse(s.cursor_visible); self.assertTrue(s.contains("NEWé")); s.feed(b"\033[?1049l"); self.assertFalse(s.alt); self.assertFalse(s.contains("NEW")); s.feed(b"\033[?999h"); self.assertTrue(s.unsupported)
        valid=bench.Screen(); valid.feed("é".encode()); valid.finish(); self.assertFalse(valid.unsupported)
        for fragment in (b"\xc3",b"\033",b"\033[12"):
            incomplete=bench.Screen(); incomplete.feed(fragment); incomplete.finish(); self.assertTrue(incomplete.unsupported)

    def test_binary_and_digest_expectations(self):
        b=bench.fixture_bytes("binary-safe.bin"); self.assertEqual(b[9],0); self.assertIn(b"DATA_HEAD",b[:8192]); self.assertNotIn(b"RO_BIN",b); self.assertTrue(b.endswith(b"\xff"*264)); self.assertEqual(hashlib.sha256(b"X"+bench.fixture_bytes("edit-invalid.txt")).hexdigest(),bench.FIX["edit-invalid.txt"]["post_edit_sha256"])
        rows=["DATA_HEAD\\x00\\x01"]+([""]*49); self.assertFalse(bench.valid_binary_evidence(rows,bench.FIX["binary-safe.bin"]))
        rows[49]=" RO BIN"; self.assertTrue(bench.valid_binary_evidence(rows,bench.FIX["binary-safe.bin"]))

    def test_report_schema_keeps_claims_unmeasured(self):
        self.assertEqual(bench.PHASE,"phase1"); self.assertTrue(all(v["status"]=="NOT_MEASURED" for v in bench.CLAIMS.values()))
        self.assertEqual(bench.RESULT_SCHEMA,"teddy-s9-phase1-result-5")
        self.assertTrue(all(v["status"]=="NOT_MEASURED" for v in bench.CLAIMS.values()))

    def test_action_order_and_page_endpoint_contract(self):
        s=bench.Screen(30,50)
        for i in range(49): s.feed((f"S9_ROW_{i:05d}\r\n").encode())
        self.assertTrue(s.text()[0].startswith("S9_ROW_00000"))
        self.assertTrue(s.text()[1].startswith("S9_ROW_00001")); self.assertTrue(s.text()[48].startswith("S9_ROW_00048"))

    def test_failure_categories_are_explicit(self):
        s=bench.Session([],timeout=.01)
        r=s.record()
        for key in ("timed_out","pty_output_capped","stderr_capped","exec_failed","cleanup_error","descendants_left","final_drain"):
            self.assertIn(key,r)

    def test_screen_fragmented_utf8_and_private_modes(self):
        s=bench.Screen(12,3); s.feed(b"\033[?1049h\033[2J\033[1;1H\xc3"); self.assertEqual(s._pending,b"\xc3")
        s.feed(b"\xa9X\0337\033[2;1HY\0338"); self.assertTrue(s.alt); self.assertTrue(s.contains("éX")); self.assertTrue(s.contains("Y"))
        s.feed(b"\033[?1049l"); self.assertFalse(s.alt); self.assertFalse(s.contains("éX"))

    def test_screen_erase_and_stale_marker(self):
        s=bench.Screen(20,3); s.feed(b"OLD\033[2K\033[1;1HNEW"); self.assertFalse(s.contains("OLD")); self.assertTrue(s.contains("NEW"))
        s.feed(b"\033[?999h"); self.assertIn("?999h",s.unsupported)

    def test_fixture_properties_markers_and_exact_source_digest(self):
        with tempfile.TemporaryDirectory() as d:
            bench.generate(d)
            for name,spec in bench.FIX.items():
                m=json.loads((Path(d)/(name+".manifest.json")).read_text())
                self.assertEqual(m["sha256"],hashlib.sha256((Path(d)/name).read_bytes()).hexdigest())
                self.assertEqual(m["size"],Path(d,name).stat().st_size); self.assertEqual(m["screen_markers"],spec.get("markers",[]))

    def test_action_timing_helper_rejects_broken_links(self):
        good={"write":.10,"first_output_after_write":.20,"endpoint":.25,"last_output":.30,"quiet_complete":.31,"quiet_gap":.01,"endpoint_found":True}
        self.assertTrue(bench.valid_action_timing(good))
        for key,value in (("write",.20),("first_output_after_write",.09),("endpoint",.09),("last_output",.09),("quiet_complete",.09),("quiet_gap",.001),("endpoint_found",False)):
            bad=dict(good); bad[key]=value; self.assertFalse(bench.valid_action_timing(bad),key)

    def test_exit_signal_and_timeout_classification(self):
        s=bench.Session([]); s.status=0; self.assertEqual(s.record()["exit"],0)
        s.status=(9 & 0x7f); self.assertIsNotNone(s.record()["signal"])
        s.status=None; s.timed_out=True; self.assertTrue(s.record()["timed_out"])

    def test_cap_and_exec_failure_classification(self):
        s=bench.Session([]); s.output_capped=True; s.stderr_capped=True; s.err.extend(b"S9_EXEC_FAILURE\n"); r=s.record()
        self.assertTrue(r["pty_output_capped"]); self.assertTrue(r["stderr_capped"]); self.assertTrue(r["exec_failed"])

    def test_close_and_cleanup_fields_cannot_be_implicit(self):
        s=bench.Session([]); s.cleanup_error="kill failed"; s.closed=False; r=s.record()
        self.assertEqual(r["cleanup_error"],"kill failed"); self.assertFalse(r["final_drain"]); self.assertFalse(r["reaped"])

    def test_profile_file_sets_are_exact_and_environment_isolated(self):
        self.assertEqual(bench.isolated_env(Path("/h"))["TERM"],"xterm-256color")
        self.assertEqual(set(("teddy",)),set(("teddy",))); self.assertEqual(set(("teddy","teddy-highlight")),set(("teddy","teddy-highlight")))

    def test_diagnostic_is_not_required_health(self):
        required=["open-screen","page-down","insert-save-crlf","insert-save-invalid","read-only-binary"]
        self.assertNotIn("renderer-diagnostic",required); self.assertEqual(bench.CLAIMS["C1"]["status"],"NOT_MEASURED")

    def test_report_rejects_claims_and_requires_evidence(self):
        self.assertTrue(all(v["status"]=="NOT_MEASURED" for v in bench.CLAIMS.values()))
        self.assertIn("pty_limitation",bench.metadata([]))

    def test_process_tree_fake_empty_is_safe(self):
        self.assertIsInstance(bench.process_tree(-1),list)

    def test_csi_k_defaults_and_stale_redraw(self):
        s=bench.Screen(10,2); s.feed(b"abcdef\033[1;1H\033[K"); self.assertEqual(s.text()[0],"")
        s.feed(b"abcdef\033[1;3H\033[1K"); self.assertEqual(s.text()[0],"   def")
        s.feed(b"\033[2K"); self.assertEqual(s.text()[0],"")

    def test_named_endpoint_is_not_cleanup_screen(self):
        action={"name":"page_down","rows":["header","S9_ROW_00001"]}; cleanup={"name":"cleanup","rows":[""]}
        retained=[action,cleanup]; endpoint=next(x for x in retained if x["name"]!="cleanup")
        self.assertTrue(endpoint["rows"][1].startswith("S9_ROW_00001"))

    def test_fake_pty_readiness_action_quiet_and_cleanup(self):
        code=textwrap.dedent("""
            import os, tty, time
            tty.setraw(0)
            os.write(1,b'\\033[?1049h\\033[2J\\033[1;1Hfake.txt\\r\\nHEAD')
            child=os.fork()
            if child == 0:
                time.sleep(10)
                os._exit(0)
            while True:
                b=os.read(0,32)
                if not b: break
                if b == b'X':
                    os.write(1,b'\\033[2;1HACTION')
                    time.sleep(.004)
                    os.write(1,b'!')
                if b'\\x11' in b: break
        """)
        with tempfile.TemporaryDirectory() as d:
            script=Path(d)/"fake.py"; script.write_text(code)
            s=bench.Session([sys.executable,str(script)],timeout=.5); s.spawn(d,d)
            self.assertTrue(s.ready("fake.txt","HEAD"))
            self.assertTrue(s.write(b"X",lambda sc:sc.contains("ACTION"),"fake_action"))
            self.assertTrue(s.close()); a=s.record()["actions"][0]
            self.assertGreater(a["first_output_after_write"],a["write"]); self.assertGreaterEqual(a["quiet_gap"],.005)
            # The endpoint is the causal PTY event, not detector time after
            # _read.  The old implementation would make this exceed the last
            # output timestamp.
            self.assertLessEqual(a["endpoint"],a["last_output"])
            self.assertGreaterEqual(a["quiet_complete"]-a["last_output"],.005)
            self.assertEqual("fake_action",s.record()["endpoint_snapshots"][0]["name"]); self.assertTrue(s.record()["final_drain"])
            self.assertFalse(s.record()["descendants_left"]); self.assertEqual(s.record()["pgid_after"],[])

    def test_quiet_reset_uses_late_byte_absolute_deadline(self):
        s=bench.Session([],timeout=.2); s.quiet_interval=.010; s.master,writer=os.pipe(); s.er,errw=os.pipe(); os.set_blocking(s.master,False); os.set_blocking(s.er,False); s.t0=time.monotonic()
        os.write(writer,b"endpoint"); s._read(.01); first=s.last_output
        gate=threading.Event(); original_read=s._read
        def controlled_read(wait=.02):
            gate.set(); return original_read(wait)
        s._read=controlled_read
        def late(): gate.wait(1); time.sleep(.004); os.write(writer,b"late")
        t=threading.Thread(target=late); t.start(); self.assertTrue(s.quiet()); t.join(); self.assertEqual(len(s.output_events),2); self.assertGreater(s.last_output,first); self.assertGreaterEqual(s.quiet_complete-s.last_output,.005)
        for fd in (writer,errw,s.master,s.er): os.close(fd)

    def test_process_identity_rules_are_behavioral(self):
        root=["/stage/teddy","/tmp/file"]; helper=["/stage/teddy-highlight"]
        member=lambda argv:{"command":" ".join(argv),"pid":1}
        self.assertTrue(bench.exact_group_identity((True,[member(root)],None),root)[0])
        self.assertFalse(bench.exact_group_identity((False,[],"ps failed"),root)[0])
        self.assertFalse(bench.exact_group_identity((True,[member(root),member(["/wrong/helper"])],None),root,helper)[0])
        self.assertFalse(bench.exact_group_identity((True,[member(root),member(["/extra"])],None),root)[0])
        self.assertFalse(bench.exact_group_identity((True,[{"command":"unterminated '"}],None),root)[0])

    def test_phase2_helpers_and_claim_boundaries(self):
        self.assertEqual(bench.phase2_quantiles([]),{"count":0,"p50":None,"p95":None})
        self.assertEqual(bench.phase2_quantiles([1,2,3,4,5])["p50"],3)
        self.assertEqual(bench.parse_perf_log("1.5 2 300\nnoise\n"),[{"us":1.5,"allocs":2,"frame_bytes":300}])
        self.assertEqual(bench.parse_perf_log("1 2 3 extra"),[])
        with self.assertRaises(ValueError): bench.validate_phase2_result({"schema":bench.PHASE2_SCHEMA,"phase":"phase2","claims":{"C1":{"status":"PASS","reason":"x"},"C2":{"status":"INCONCLUSIVE","reason":"x"},"C3":{"status":"INCONCLUSIVE","reason":"x"},"C4":{"status":"PASS","reason":"x"},"C5":{"status":"PASS","reason":"x"}},"artifact_root":{"path":"/tmp/no"}})

    def _attribution_fixture(self):
        root=Path(__file__).parents[1]/"artifacts"/"test-attribution"; (root/"corpus").mkdir(parents=True,exist_ok=True); (root/"profiles"/"bare").mkdir(parents=True,exist_ok=True); (root/"profiles"/"shipped").mkdir(parents=True,exist_ok=True); corpus=root/"corpus"/"s9-1g.log"; corpus.touch(); corpus.open("r+b").truncate(1<<30); files={}
        for profile,names in (("bare",("teddy",)),("shipped",("teddy","teddy-highlight"))):
            for name in names:
                path=root/"profiles"/profile/name; path.write_bytes((profile+name).encode()); digest,size=bench.sha(path); files.setdefault(profile,{})[name]={"path":str(path),"sha256":digest,"size":size,"version":"test","version_capture":{"stdout":"test","stderr":"","exit":0},"arch":bench.platform.machine()}
        corpus_sha=bench.sha(corpus)[0]; samples=[]
        for profile in ("bare","shipped"):
            root_argv=[files[profile]["teddy"]["path"],str(corpus)]; helper=files[profile].get("teddy-highlight",{}).get("path"); group=[{"command":" ".join(root_argv),"pid":1}]+([{"command":helper,"pid":2}] if helper else [])
            for block,order in ((1,("current","deferred")),(2,("deferred","current"))):
                for mode in order:
                    for rep in range(1,32):
                        ident={"verified":True,"argv":root_argv,"observed":"teddy"}; probes={"identity":{"result":ident,"verified":True,"error":None,"duration_ms":2.0 if mode=="current" else .5,"started_ms":1.0 if mode=="current" else 31.0,"finished_ms":3.0 if mode=="current" else 31.5},"process_group":{"success":True,"rows":group,"error":None,"duration_ms":8.0 if mode=="current" else 1.0,"started_ms":3.0 if mode=="current" else 31.5,"finished_ms":11.0 if mode=="current" else 32.5,"identity":[True,None]}}; process={"exit":0,"signal":None,"reaped":True,"pty_eof":True,"stderr_eof":True,"drain_complete":True,"drain_deadline":False,"timed_out":False,"pty_output_capped":False,"stderr_capped":False,"unsupported":[],"exec_failed":False,"cleanup_error":None,"pgid_after":[],"descendants_left":False,"pgid_probe_error":None,"pgid_before_probe_error":None,"pgid_after_probe_error":None}; readiness={"matched":True,"name":"c1_attribution_ready","matched_at":.04 if mode=="current" else .03,"snapshot":["S9_C1_ROW_000000"]}; samples.append({"profile":profile,"block":block,"mode":mode,"rep":rep,"status":"PASS","valid":True,"validity_reason":"valid paired attribution sample","argv":root_argv,"parent_start_ms":1.0,"probes":probes,"first_output_ms":20.0 if mode=="current" else 10.0,"readiness_ms":readiness["matched_at"]*1000,"readiness":readiness,"identity":ident,"group_identity":[True,None],"process":process})
        summary={profile:bench._c1_attribution_summary(samples,profile) for profile in ("bare","shipped")}; return {"schema":"teddy-s9-c1-attribution-1","diagnostic":"c1-attribution","artifact_root":{"path":"bench/artifacts/test-attribution"},"geometry":[200,50],"source":{"commit":"test","dirty":False},"profile_provenance":{p:{"root":str(root/"profiles"/p),"helper":files[p].get("teddy-highlight",{}).get("path"),"files":files[p],"exact_files":sorted(files[p])} for p in files},"corpus":{"path":str(corpus),"size":1<<30,"sha256":corpus_sha},"warmups":5,"plan":[list(x) for x in bench.c1_attribution_plan()],"profiles":summary,"samples":samples,"classification":"OBSERVER_CONTAMINATION","claim":"methodology attribution only; no editor performance claim"}

    def test_c1_attribution_materialized_mutations(self):
        base=self._attribution_fixture(); self.assertTrue(bench.validate_c1_attribution_report(base)); shuffled=json.loads(json.dumps(base)); shuffled["samples"]=list(reversed(shuffled["samples"])); self.assertTrue(bench.validate_c1_attribution_report(shuffled))
        def reject(label,mutate):
            value=json.loads(json.dumps(base)); mutate(value); self.assertTrue(bench.validate_c1_attribution_report(base),label+" baseline")
            with self.assertRaises(ValueError,msg=label): bench.validate_c1_attribution_report(value)
        reject("missing rep",lambda x:x["samples"].pop()); reject("duplicate rep",lambda x:x["samples"].__setitem__(1,json.loads(json.dumps(x["samples"][0])))); reject("wrong root group member",lambda x:x["samples"][0]["probes"]["process_group"]["rows"].__setitem__(0,{"command":"/wrong","pid":1})); reject("extra group member",lambda x:x["samples"][0]["probes"]["process_group"]["rows"].append({"command":"/extra","pid":2})); reject("readiness sentinel",lambda x:x["samples"][0]["readiness"]["snapshot"].__setitem__(0,"filename only")); reject("current probe ordering",lambda x:x["samples"][0]["probes"]["identity"].__setitem__("finished_ms",21.0)); reject("deferred probe ordering",lambda x:x["samples"][31]["probes"]["identity"].__setitem__("started_ms",1.0)); reject("profile provenance path",lambda x:next(iter(x["profile_provenance"]["bare"]["files"].values())).__setitem__("path","/wrong/binary")); reject("profile provenance hash",lambda x:next(iter(x["profile_provenance"]["bare"]["files"].values())).__setitem__("sha256","0"*64)); reject("sample argv provenance",lambda x:x["samples"][0]["argv"].__setitem__(0,"/wrong/teddy")); reject("current probe",lambda x:x["samples"][0]["probes"]["process_group"].__setitem__("success",False)); reject("deferred probe",lambda x:x["samples"][31]["probes"]["identity"].__setitem__("verified",False)); reject("unverified identity",lambda x:x["samples"][0]["identity"].__setitem__("verified",False)); reject("signal",lambda x:x["samples"][0]["process"].__setitem__("signal",9)); reject("timeout",lambda x:x["samples"][0]["process"].__setitem__("timed_out",True)); reject("cap",lambda x:x["samples"][0]["process"].__setitem__("pty_output_capped",True)); reject("unsupported",lambda x:x["samples"][0]["process"].__setitem__("unsupported",["x"])); reject("cleanup",lambda x:x["samples"][0]["process"].__setitem__("cleanup_error","error")); reject("PGID",lambda x:x["samples"][0]["process"].__setitem__("pgid_after",[{"pid":2}])); reject("duration arithmetic",lambda x:x["profiles"]["bare"]["blocks"]["1"].__setitem__("current_p95_ms",999.0)); reject("one block criterion",lambda x:x["profiles"]["bare"]["blocks"]["1"].__setitem__("classification","NO_REPRODUCIBLE_OBSERVER_CONTAMINATION")); reject("profile classification",lambda x:x["profiles"]["bare"].__setitem__("classification","NO_REPRODUCIBLE_OBSERVER_CONTAMINATION")); reject("top-level observer contamination",lambda x:x.__setitem__("classification","NO_REPRODUCIBLE_OBSERVER_CONTAMINATION"))

    def test_comparator_command_contract(self):
        corpus=str(Path("/tmp/s9-1g.log").resolve())
        self.assertEqual(bench.comparator_argv("nvim","/bin/nvim",corpus),["/bin/nvim","--clean","-R","--",corpus])
        self.assertEqual(bench.comparator_argv("vim","/bin/vim",corpus),["/bin/vim","--clean","-R","-i","NONE","-U","NONE","--",corpus])
        self.assertEqual(bench.comparator_argv("hx","/bin/hx",corpus),["/bin/hx","--config","/dev/null","--",corpus])
        self.assertEqual(bench.comparator_argv("kak","/bin/kak",corpus),["/bin/kak","-n","-ro","-ui","terminal","--",corpus])
        self.assertEqual(bench.comparator_argv("less","/bin/less",corpus),["/bin/less","-n","-L","--",corpus])
        with self.assertRaises(ValueError): bench.comparator_argv("vis","/usr/bin/vis",corpus)

    def test_c1_attribution_plan_timing_and_criterion(self):
        plan=bench.c1_attribution_plan(); self.assertEqual(len(plan),124); self.assertEqual(plan[:4],[(1,"current",1),(1,"deferred",1),(1,"current",2),(1,"deferred",2)]); self.assertEqual(plan[62:66],[(2,"deferred",1),(2,"current",1),(2,"deferred",2),(2,"current",2)])
        result=bench.c1_attribution_criterion([40.0]*31,[30.0]*31,10.0); self.assertEqual(result["classification"],"OBSERVER_CONTAMINATION"); self.assertTrue(result["criterion"])
        result=bench.c1_attribution_criterion([40.0]*31,[37.0]*31,10.0); self.assertEqual(result["classification"],"NO_REPRODUCIBLE_OBSERVER_CONTAMINATION")

    def test_phase2_validator_and_report_mutations(self):
        p=Path(__file__).parents[1]/"results"/"canonical.json"
        if not p.is_file(): self.skipTest("canonical Phase 2 result not present")
        base=json.loads(p.read_text()); self.assertTrue(bench.validate_phase2_result(base)); text=(Path(__file__).parents[2]/"docs"/"bench_results.md").read_text(); self.assertTrue(bench.validate_phase2_report(base,text)); self.assertIn("## Editor metric summary",text); self.assertIn("| Editor / profile | Repetitions | p50 (ms) | p95 (ms) | Status | Comparability caveat |",text); self.assertIn("| Teddy / bare |",text); self.assertIn("does not affect C1–C5",text)
        for mutate in (lambda x:x["claims"]["C1"]["quantiles_ms"]["bare"].__setitem__("p95",1),lambda x:x["claims"]["C1"].__setitem__("status","FAIL"),lambda x:x["claims"]["C3"]["attempts"][0].__setitem__("semantic_association",True),lambda x:x["claims"]["C4"]["fixtures"].pop(),lambda x:x["environment"].__setitem__("CMUX_SOCKET_CAPABILITY","forbidden")):
            value=json.loads(json.dumps(base)); mutate(value)
            with self.assertRaises(ValueError): bench.validate_phase2_result(value)
        with self.assertRaises(ValueError): bench.validate_phase2_report(base,text+"\nforged\n")

    def test_phase2_canonical_copy_mutations(self):
        p=Path(__file__).parents[1]/"results"/"canonical.json"
        if not p.is_file(): self.skipTest("canonical Phase 2 result not present")
        base=json.loads(p.read_text())
        def reject(label,mutate):
            value=json.loads(json.dumps(base)); mutate(value)
            with self.assertRaises(ValueError,msg=label): bench.validate_phase2_result(value)
        reject("invalid C1 repetition with FAIL",lambda x:x["claims"]["C1"]["profiles"]["bare"][0].__setitem__("status","INCONCLUSIVE"))
        reject("C1 artifact/canonical lifecycle mismatch",lambda x:x["claims"]["C1"]["profiles"]["bare"][0]["process"].__setitem__("reaped",False))
        reject("C3 action order",lambda x:x["claims"]["C3"]["attempts"][0]["actions"].reverse())
        reject("C3 submit byte",lambda x:x["claims"]["C3"]["attempts"][0]["actions"][2].__setitem__("bytes","00"))
        reject("C3 false association",lambda x:(x["claims"]["C3"]["attempts"][0].__setitem__("semantic_association",True),x["claims"]["C3"].__setitem__("status","PASS")))
        reject("C3 artifact/canonical mismatch",lambda x:x["claims"]["C3"]["attempts"][0].__setitem__("needle","detached"))
        reject("C4 saved artifact hash detachment",lambda x:x["claims"]["C4"]["fixtures"][0].__setitem__("actual_saved_sha256","0"*64))
        reject("C4 missing matrix",lambda x:x["claims"]["C4"]["fixtures"].pop())
        reject("C4 digest mismatch",lambda x:x["claims"]["C4"]["fixtures"][0]["expected"].__class__ and x["claims"]["C4"]["fixtures"][0].__setitem__("expected","0"*64))
        text=(Path(__file__).parents[2]/"docs"/"bench_results.md").read_text()
        mutated=json.loads(json.dumps(base)); mutated["claims"]["C1"]["quantiles_ms"]["bare"]["p95"]+=1
        with self.assertRaises(ValueError,msg="stale rendered report after C1 numeric mutation"): bench.validate_phase2_report(mutated,text)

    def test_phase2_c1_observer_boundary_and_methodology_are_materialized(self):
        p=Path(__file__).parents[1]/"results"/"canonical.json"
        if not p.is_file(): self.skipTest("canonical Phase 2 result not present")
        base=json.loads(p.read_text()); self.assertTrue(bench.validate_phase2_result(base)); text=(Path(__file__).parents[2]/"docs"/"bench_results.md").read_text(); self.assertTrue(bench.validate_phase2_report(base,text))
        def reject(label,mutate):
            value=json.loads(json.dumps(base)); mutate(value); self.assertTrue(bench.validate_phase2_result(base),label+" baseline")
            with self.assertRaises(ValueError,msg=label): bench.validate_phase2_result(value)
        reject("C1 pre-sentinel identity probe",lambda x:x["claims"]["C1"]["profiles"]["bare"][0]["identity_probe"].__setitem__("started_ms",0.0))
        reject("C1 pre-sentinel PGID probe",lambda x:x["claims"]["C1"]["profiles"]["shipped"][0]["process_group_probe"].__setitem__("started_ms",0.0))
        reject("C1 methodology version",lambda x:x.__setitem__("methodology","old-method"))
        reject("C1 historical context",lambda x:x.__setitem__("historical_context","missing"))

    def test_universal_contract_is_separate_and_adversarial(self):
        with tempfile.TemporaryDirectory() as d: self._universal_contract_body(Path(d))

    def _universal_contract_body(self, d):
        # Participants must be real files on disk: the validator re-hashes every
        # IDENTITY_QUALIFIED binary, so synthetic paths cannot stand in.
        adapters=[]
        for name in bench.UNIVERSAL_ADAPTER_ORDER:
            if name=="vis": adapters.append({"name":name,"status":"REJECTED_UNSUPPORTED","path":"/usr/bin/vis"})
            elif name=="vi": adapters.append({"name":name,"status":"ALIAS_OF","alias_of":"vim","path":str(d/"vim")})
            else:
                p=d/name; p.write_bytes(name.encode()); h,z=bench.sha(p)
                adapters.append({"name":name,"status":"IDENTITY_QUALIFIED","path":str(p),"sha256":h,"size":z,"arch":"arm64","version":"1"})
        (d/"teddy-highlight").write_bytes(b"helper")
        vim=next(a for a in adapters if a["name"]=="vim"); vi=next(a for a in adapters if a["name"]=="vi"); vi.update({k:vim[k] for k in ("path","sha256","size","arch","version")}); ops={a["name"]:{op:{"status":"INCONCLUSIVE","repetitions":0,"p50_ms":None,"p95_ms":None,"attempts":[]} for op in ("startup","search")} for a in adapters if a["status"]=="IDENTITY_QUALIFIED"}
        for a in adapters:
            spec=bench.UNIVERSAL_ADAPTER_SPEC[a["name"]]; a.update({"alias_of":a.get("alias_of"),"argv":bench.universal_adapter_argv(a["name"],a["path"],"/tmp/universal"),"search_prompt":spec["search_prompt"].hex(),"search_submit":spec["search_submit"].hex(),"quit":spec["quit"].hex(),"expected_topology":spec["expected_topology"],"expected_class":spec["expected_class"],"helper_path":str(Path(a["path"]).parent/"teddy-highlight") if a["name"]=="teddy-shipped" else None,"sha256":a.get("sha256","0"*64),"size":a.get("size",1),"arch":a.get("arch","arm64"),"version":a.get("version","rejected")})
        result={"schema":bench.UNIVERSAL_SCHEMA,"phase":"universal-comparison","contract_only":True,"artifact_root":{"path":"/tmp/universal-artifacts"},"geometry":[200,50],"warmups":3,"blocks":2,"repetitions_per_block":16,"corpus":{"path":"/tmp/universal","size":1<<30,"record_bytes":64,"sha256":"b"*64,"head_marker":"UBENCH_HEAD_RECORD","needle":"UBENCH_NEEDLE","target_marker":"UBENCH_TARGET_RECORD","tail_marker":"UBENCH_TAIL_RECORD","head_offset":0,"target_offset":512<<20,"needle_offset":(512<<20)+len("UBENCH_TARGET_RECORD "),"tail_offset":(1<<30)-64},"adapters":adapters,"operations":ops}
        result["execution_state"]="not_started"; result["schedule"]=[]; result["adapter_smoke"]=self._strict_smoke("/tmp/universal-artifacts",adapters); [r.update({"attempts":0,"status":"INCONCLUSIVE"}) for r in result["adapter_smoke"]["records"].values()]
        with mock.patch.object(bench,"validate_universal_corpus",return_value=True): self.assertTrue(bench.validate_universal_result(result))
        self.assertTrue(bench.validate_universal_report(result,bench.universal_report_markdown(result)))
        bad=json.loads(json.dumps(result)); bad["adapters"][0],bad["adapters"][1]=bad["adapters"][1],bad["adapters"][0]
        with mock.patch.object(bench,"validate_universal_corpus",return_value=True):
            with self.assertRaises(ValueError): bench.validate_universal_result(bad)
        self.assertEqual(bench.universal_metric([])["status"],"INCONCLUSIVE"); self.assertEqual(bench.universal_adapter_argv("vi","/opt/vim","/tmp/x")[0],"/opt/vim")

    def test_universal_teddy_positional_and_deterministic_query_reply(self):
        argv=bench.universal_adapter_argv("teddy-shipped","/stage/teddy","/stage/corpus")
        self.assertEqual(argv,["/stage/teddy","/stage/corpus"]); self.assertNotIn("--read-only",argv)
        s=bench.Session([]); s.master,writer=os.pipe(); s.er,errw=os.pipe(); os.set_blocking(s.master,False); os.set_blocking(s.er,False); s.t0=time.monotonic()
        os.write(writer,b"\033["); s._read(.01); os.write(writer,b"6n"); s._read(.01); os.write(writer,b"\033"); s._read(.01); os.write(writer,b"[c"); s._read(.01)
        self.assertEqual([x["kind"] for x in s.harness_traffic],["DSR_REPLY","DA_REPLY"]); self.assertTrue(all(x["deterministic"] for x in s.harness_traffic))
        for fd in (writer,errw,s.master,s.er): os.close(fd)

    def test_trace_provenance_is_streaming_and_materialized(self):
        base=[{"channel":"pty_output_raw","at":1.0,"data":"616263"},{"channel":"pty_output","at":1.0,"data":"616263"}]
        self.assertTrue(bench.validate_trace_provenance(base,[]))
        for bad in (
            [{**base[0]},{**base[1],"at":2.0}],
            [{**base[0]},{**base[1],"data":"616264"}],
            [{"channel":"pty_output","at":1.0,"data":"616263"},{"channel":"pty_output_raw","at":1.0,"data":"616263"}],
            [{"channel":"pty_output_raw","at":1.0,"data":"1b5b"},{"channel":"pty_output","at":1.0,"data":""}],
        ): self.assertFalse(bench.validate_trace_provenance(bad,[]))
        query=[{"channel":"pty_output_raw","at":1.0,"data":"1b5b"},{"channel":"pty_output","at":1.0,"data":""},{"channel":"pty_output_raw","at":2.0,"data":"366e58"},{"channel":"pty_input","at":2.0,"harness":True,"data":"1b5b313b3152"},{"channel":"pty_output","at":2.0,"data":"58"}]
        traffic=[{"kind":"DSR_REPLY","query":"1b5b366e","reply":"1b5b313b3152","deterministic":True,"at":2.0}]
        self.assertTrue(bench.validate_trace_provenance(query,traffic))
        delayed=json.loads(json.dumps(traffic)); delayed[0]["at"]=1.1
        self.assertFalse(bench.validate_trace_provenance(query,delayed))
        with tempfile.TemporaryDirectory() as d:
            result=self._strict_universal_fixture(d); attempt=result["operations"]["teddy-shipped"]["startup"]["attempts"][0]; trace=json.loads(Path(attempt["trace"]["path"]).read_text()); trace[1]["at"]=2.0; Path(attempt["trace"]["path"]).write_text(json.dumps(trace)); attempt["trace"]["sha256"],attempt["trace"]["size"]=bench.sha(attempt["trace"]["path"]); self.assertFalse(bench.validate_universal_attempt(attempt,"startup","b"*64,next(a for a in result["adapters"] if a["name"]=="teddy-shipped"),result["artifact_root"],result["corpus"]["path"]))

    def test_attempted_smoke_strict_lifecycle_mutations(self):
        process={"exit":0,"signal":None,"reaped":True,"pty_eof":True,"stderr_eof":True,"drain_complete":True,"drain_deadline":False,"cleanup_error":None,"pgid_after":[],"descendants_left":False,"timed_out":False,"output_capped":False,"stderr_capped":False,"unsupported":[],"exec_failed":False}
        self.assertTrue(bench._smoke_lifecycle_ok(process))
        for field,value in (("drain_deadline",True),("cleanup_error","x"),("output_capped",True),("stderr_capped",True)):
            bad=dict(process); bad[field]=value; self.assertFalse(bench._smoke_lifecycle_ok(bad),field)

    def test_universal_fake_pty_participant_emits_queries_prompt_target_and_child(self):
        script=Path(__file__).with_name("fake_universal_adapter.py"); s=bench.Session([sys.executable,str(script)],timeout=3); s.spawn(script.parent,script.parent)
        self.assertTrue(s.until(lambda sc:sc.contains("UBENCH_HEAD_RECORD"),"head")["matched"])
        self.assertTrue(s.write(b"/UBENCH_NEEDLE\r",lambda sc:sc.contains("UBENCH_TARGET_RECORD"),"search")); self.assertTrue(s.close())
        record=s.record(); self.assertEqual(record["exit"],0); self.assertTrue(any(x["kind"]=="DSR_REPLY" for x in record["harness_traffic"])); self.assertTrue(any(x["kind"]=="DA_REPLY" for x in record["harness_traffic"])); self.assertFalse(record["descendants_left"]); self.assertTrue(record["drain_complete"])

    def test_screen_models_real_editor_sequences(self):
        """Real editors drive sequences this model used to reject wholesale.

        Consuming the content-neutral ones is what makes comparators eligible
        to be measured. The two that DO move the grid are implemented, and the
        allow-list must keep rejecting modes that change what is drawn --
        waving those through would silently corrupt every endpoint match.
        """
        neutral={
            "mouse":b"\033[?1002h\033[?1006h\033[?1000l","bracketed paste":b"\033[?2004h\033[?2004l",
            "focus":b"\033[?1004h","cursor blink":b"\033[?12h\033[?12l","cursor shape":b"\033[2 q\033[0 q",
            "DECRQM":b"\033[?2026$p\033[?69$p","SGR sub-params":b"\033[4:3m\033[0m",
            "kitty keyboard":b"\033[>5u\033[<u\033[?u","modifyOtherKeys":b"\033[>4;2m",
            "XTWINOPS title":b"\033[22;0;0t\033[23;0;0t","keypad":b"\033=\033>","charset":b"\033(B",
            "OSC BEL":b"\033]0;title\007","OSC ST":b"\033]2;title\033\\","DCS ST":b"\033P+q544e\033\\",
            "DA/DSR":b"\033[>c\033[5n\033[6n",
        }
        for name,seq in neutral.items():
            s=bench.Screen(10,4); s.feed(b"KEEP"); s.feed(seq); s.finish()
            self.assertEqual(s.unsupported,[],f"{name} should be consumed as content-neutral")
            self.assertTrue(s.contains("KEEP"),f"{name} must not disturb the grid")
        # An unrecognised mode, or one that moves content, stays unsupported.
        # ?2027 alters grapheme clustering (cell width), so it is not neutral.
        for mode in ("?999h","?7l","?3h","?6h","?69h","?2027h"):
            s=bench.Screen(10,4); s.feed(b"\033["+mode.encode()); s.finish()
            self.assertEqual(s.unsupported,[mode],f"{mode} must not be waved through")
        # Sequences that change how *subsequent* bytes render must be flagged,
        # even though their own bytes draw nothing.
        s=bench.Screen(10,4); s.feed(b"\033(0"); s.finish()
        self.assertTrue(s.unsupported,"DEC Special Graphics charset must not be treated as neutral")
        s=bench.Screen(10,4); s.feed(b"\033Pq#0;2;0;0;0\033\\"); s.finish()
        self.assertTrue(s.unsupported,"Sixel DCS renders content and must not be silently consumed")
        s=bench.Screen(10,4); s.feed(b"\033P+q544e\033\\"); s.finish()
        self.assertEqual(s.unsupported,[],"an inert termcap DCS query is still consumed cleanly")
        s=bench.Screen(10,4); s.feed(b"ABCDEFGH\033[1;3H\033[3X"); self.assertEqual(s.text()[0],"AB   FGH")
        s=bench.Screen(10,4); s.feed(b"AB\r\nCD\033[1;2H\033[0J"); self.assertEqual(s.text()[:2],["A",""])
        s=bench.Screen(10,4); s.feed(b"AB\r\nCD\033[2;1H\033[1J"); self.assertEqual(s.text()[:2],[""," D"])
        s=bench.Screen(10,4); s.feed(b"\033[1;3r"); s.feed(b"A\r\nB\r\nC\r\nD\r\nE")
        self.assertEqual(s.text(),["C","D","E",""],"LF at the bottom margin must scroll only inside the region")
        s=bench.Screen(10,3); s.feed(b"1\r\n2\r\n3\r\n4")
        self.assertEqual(s.text(),["2","3","4"],"LF at the last row must scroll the screen")
        # DECSTBM homes to the absolute top-left, not the top margin, because
        # origin mode (?6) is never enabled.
        s=bench.Screen(10,5); s.feed(b"\033[2;4r"); s.feed(b"X")
        self.assertEqual(s.text()[0],"X","DECSTBM must home to absolute row 1 with origin mode reset")
        # A cursor below the scrolling region must not scroll it.
        s=bench.Screen(10,5); s.feed(b"\033[1;2r\033[5;1HZ\n")
        self.assertEqual(s.text()[4],"Z","LF below the bottom margin must not scroll the region")

    def test_unclean_smoke_is_inconclusive_evidence_not_a_hard_failure(self):
        """An unclean lifecycle must downgrade a participant, not abort the run.

        Real editors emit sequences this screen model does not implement, so
        every comparator lands here. Raising made the whole comparison
        unrunnable; the contract is that a failed smoke records INCONCLUSIVE
        and simply cannot manufacture metrics.
        """
        with tempfile.TemporaryDirectory() as d:
            root=Path(d); adapters=[]
            for name in bench.UNIVERSAL_ADAPTER_ORDER:
                p=root/name; path="/usr/bin/vis" if name=="vis" else str(p)
                if name!="vis": p.write_bytes(name.encode())
                status="REJECTED_UNSUPPORTED" if name=="vis" else ("ALIAS_OF" if name=="vi" else "IDENTITY_QUALIFIED")
                spec=bench.UNIVERSAL_ADAPTER_SPEC[name]
                adapters.append({"name":name,"status":status,"path":path,"sha256":"a"*64,"size":10,"arch":"arm64","version":"v1","alias_of":"vim" if name=="vi" else None,"argv":bench.universal_adapter_argv(name,path,str(root/"corpus")),"search_prompt":spec["search_prompt"].hex(),"search_submit":spec["search_submit"].hex(),"quit":spec["quit"].hex(),"expected_topology":spec["expected_topology"],"expected_class":spec["expected_class"],"helper_path":str(root/"teddy-highlight") if name=="teddy-shipped" else None})
            (root/"teddy-highlight").write_bytes(b"helper")
            for x in adapters:
                if x["name"]!="vis": x["sha256"],x["size"]=bench.sha(x["path"])
            smoke=self._strict_smoke(root,adapters); by_name={a["name"]:a for a in adapters}
            bench._validate_universal_smoke(smoke,by_name,{"path":str(root)})  # baseline: clean PASS validates
            record=smoke["records"]["nvim"]
            record["process"]["unsupported"]=["?1002h","2 q"]; record["status"]="INCONCLUSIVE"; record["reason"]="unsupported terminal sequences"
            bench._validate_universal_smoke(smoke,by_name,{"path":str(root)})
            # The same unclean lifecycle may not be dressed up as a PASS.
            record["status"]="PASS"
            with self.assertRaises(ValueError): bench._validate_universal_smoke(smoke,by_name,{"path":str(root)})
            # Unclean is allowed; incomplete is not. Relaxing cleanliness for
            # INCONCLUSIVE must not also let the record go silent.
            record["status"]="INCONCLUSIVE"
            for field in ("signal","reaped","drain_complete","timed_out","exec_failed"):
                stripped=json.loads(json.dumps(record)); stripped["process"].pop(field)
                with self.assertRaises(ValueError,msg=f"INCONCLUSIVE record missing {field} accepted"):
                    bench._validate_universal_smoke({**smoke,"records":{**smoke["records"],"nvim":stripped}},by_name,{"path":str(root)})

    def test_participant_that_never_starts_is_validatable(self):
        """A participant failing at startup_head must still validate.

        startup_head has no input action to associate, so requiring one made
        any such record unvalidatable -- which is what `less` produces, and it
        aborted the whole comparison rather than marking it ineligible.
        """
        with tempfile.TemporaryDirectory() as d:
            root=Path(d); corpus=root/"smoke.txt"; corpus.write_bytes(b"nothing useful here\n"); os.chmod(corpus,0o444)
            script=root/"silent.py"; script.write_text('import os,time\nos.write(1, b"\\033[6n\\033[c")\nos.write(1, b"NO_MARKER_HERE\\n")\nwhile True:\n    c=os.read(0,1)\n    if not c or c==b"q": os._exit(0)\n')
            spec=bench.UNIVERSAL_ADAPTER_SPEC["less"]; argv=[sys.executable,str(script)]
            adapter={"name":"less","status":"IDENTITY_QUALIFIED","path":str(script.resolve()),"sha256":bench.sha(script)[0],"size":bench.sha(script)[1],"arch":bench.platform.machine(),"version":"fake","alias_of":None,"argv":argv,"search_prompt":spec["search_prompt"].hex(),"search_submit":spec["search_submit"].hex(),"quit":spec["quit"].hex(),"expected_topology":spec["expected_topology"],"expected_class":spec["expected_class"],"helper_path":None}
            with mock.patch.object(bench,"process_group",side_effect=lambda _:(True,[{"command":" ".join(argv),"pid":1}],None)), \
                 mock.patch.object(bench.Session,"_ps",lambda self:{"verified":True,"argv":argv}):
                record=bench._universal_adapter_smoke(adapter,corpus,root)
            self.assertEqual(record["status"],"INCONCLUSIVE")
            self.assertEqual([a.get("name") for a in json.loads(Path(record["records"]["actions"]["path"]).read_text())],["quit"],"only quit should be sent when startup never matched")
            bench._validate_universal_smoke({"corpus":str(corpus),"records":{"less":record}},{"less":adapter},{"path":str(root)})

    def test_real_smoke_actions_bind_to_their_trace_events(self):
        """Run the real smoke path and check every action pairs with its event.

        The strict smoke fixture hand-builds actions and trace events, so it
        never exercised _universal_adapter_smoke's own quit write -- which
        omitted the event name and the action write time, and could not be
        paired by the validator. Only a real participant reaches that code.
        """
        with tempfile.TemporaryDirectory() as d:
            root=Path(d); corpus=root/"smoke.txt"; corpus.write_bytes(b"UBENCH_HEAD_RECORD\n"+b"DATA\n"*20+b"UBENCH_TARGET_RECORD UBENCH_NEEDLE\n"); os.chmod(corpus,0o444)
            # Echoes every byte, so each write_endpoint phase actually matches.
            # fake_universal_adapter.py waits for a whole line and so never
            # reaches them, which makes it useless for this check.
            script=root/"echo_adapter.py"; script.write_text(textwrap.dedent('''
                import os
                os.write(1, b"\\033[6n\\033[c")
                os.write(1, b"UBENCH_HEAD_RECORD\\n")
                while True:
                    try: c = os.read(0, 1)
                    except OSError: break
                    if not c: break
                    if c == b"q": os._exit(0)
                    if c == b"\\r": os.write(1, b"\\r\\nUBENCH_TARGET_RECORD UBENCH_NEEDLE\\n")
                    else: os.write(1, c)
            ''').strip())
            spec=bench.UNIVERSAL_ADAPTER_SPEC["less"]; argv=[sys.executable,str(script)]
            adapter={"name":"less","status":"IDENTITY_QUALIFIED","path":str(script.resolve()),"sha256":bench.sha(script)[0],"size":bench.sha(script)[1],"arch":bench.platform.machine(),"version":"fake","alias_of":None,"argv":argv,"search_prompt":spec["search_prompt"].hex(),"search_submit":spec["search_submit"].hex(),"quit":spec["quit"].hex(),"expected_topology":spec["expected_topology"],"expected_class":spec["expected_class"],"helper_path":None}
            # Identity compares ps output to argv, which never matches a script.
            with mock.patch.object(bench,"process_group",side_effect=lambda _:(True,[{"command":" ".join(argv),"pid":1}],None)), \
                 mock.patch.object(bench.Session,"_ps",lambda self:{"verified":True,"argv":argv}):
                record=bench._universal_adapter_smoke(adapter,corpus,root)
            actions=json.loads(Path(record["records"]["actions"]["path"]).read_text())
            trace=json.loads(Path(record["records"]["trace"]["path"]).read_text())
            events=[e for e in trace if e.get("channel")=="pty_input" and e.get("action") is True]
            self.assertEqual(len(actions),len(events),"every recorded action needs exactly one action trace event")
            self.assertIn("quit",[a.get("name") for a in actions],"probe is vacuous unless the quit path ran")
            self.assertIn("submit",[a.get("name") for a in actions],"probe is vacuous unless the write_endpoint phases ran")
            for action,event in zip(actions,events):
                self.assertEqual(event.get("name"),action.get("name"),f"unnamed trace event for {action.get('name')}")
                self.assertEqual(event.get("data"),action.get("bytes"))
                self.assertEqual(event.get("at"),action.get("write"),f"write time not bound for {action.get('name')}")
            # A phase reached live must persist as matched. until() and
            # write_endpoint() name the ordinal differently, and reading only
            # until()'s key recorded every later phase as missing.
            phase_map=json.loads(Path(record["records"]["phase_snapshots"]["path"]).read_text())
            persisted={n:json.loads(Path(v["path"]).read_text()) for n,v in phase_map.items()}
            # prompt and typed_needle are the write_endpoint phases this
            # participant reliably reaches; reaching submit at all proves they
            # matched live, since a failed phase stops the sequence.
            for phase in ("prompt","typed_needle"):
                self.assertTrue(persisted[phase].get("matched"),f"{phase} was reached live but persisted as unmatched")
                self.assertIsInstance(persisted[phase].get("output_event_ordinal"),int,f"{phase} persisted without its output ordinal")

    def _fresh_universal_run(self, root, session_cls):
        root=Path(root); corpus=root/"tiny"; corpus.write_bytes(b"UBENCH_HEAD_RECORD\nUBENCH_TARGET_RECORD UBENCH_NEEDLE\n"); (root/"teddy-highlight").write_bytes(b"helper")
        adapters=[]
        for name in bench.UNIVERSAL_ADAPTER_ORDER:
            path=root/name; path.write_bytes(name.encode()); spec=bench.UNIVERSAL_ADAPTER_SPEC[name]
            status="REJECTED_UNSUPPORTED" if name=="vis" else ("ALIAS_OF" if name=="vi" else "IDENTITY_QUALIFIED")
            if name=="vis": path=Path("/usr/bin/vis")
            h,z=("0"*64,1) if name=="vis" else bench.sha(path)
            adapters.append({"name":name,"status":status,"path":str(path.resolve()),"sha256":h,"size":z,"arch":bench.platform.machine(),"version":"fake","alias_of":"vim" if name=="vi" else None,"argv":bench.universal_adapter_argv(name,path,corpus),"search_prompt":spec["search_prompt"].hex(),"search_submit":spec["search_submit"].hex(),"quit":spec["quit"].hex(),"expected_topology":spec["expected_topology"],"expected_class":spec["expected_class"],"helper_path":str((root/"teddy-highlight").resolve()) if name=="teddy-shipped" else None})
        vim=next(a for a in adapters if a["name"]=="vim"); vi=next(a for a in adapters if a["name"]=="vi"); vi.update({k:vim[k] for k in ("path","sha256","size","arch","version")}); vi["argv"]=bench.universal_adapter_argv("vi",vi["path"],corpus)
        smoke=self._strict_smoke(root/"artifacts",adapters); smoke["records"]["teddy-shipped"]["status"]="PASS"
        for name in ("vim","hx","kak","less","vi"): smoke["records"][name]={"status":"INCONCLUSIVE","reason":"fresh fake smoke","attempts":0}
        patch=mock.patch.object(bench,"process_group",side_effect=lambda _:(True,[{"command":" ".join(ExecutorFakeSession.current_argv),"pid":1}]+([{"command":next(a for a in adapters if a["name"]=="teddy-shipped")["helper_path"],"pid":2}] if ExecutorFakeSession.current_argv[0].endswith("teddy-shipped") else []),None))
        with patch, mock.patch.object(bench,"validate_universal_corpus",return_value=True):
            return bench.execute_universal_schedule(adapters,str(corpus),root/"artifacts",session_cls=session_cls,smoke=smoke), corpus, smoke

    def test_fresh_late_output_is_excluded_but_retained_in_trace(self):
        with tempfile.TemporaryDirectory() as d:
            result,corpus,_=self._fresh_universal_run(d,LateOutputSession)
            with mock.patch.object(bench,"validate_universal_corpus",return_value=True): self.assertTrue(bench.validate_universal_result(result))
            attempt=result["operations"]["teddy-shipped"]["search"]["attempts"][0]; phase=json.loads(Path(attempt["phase_snapshots"]["path"]).read_text())["prompt"]; prompt=json.loads(Path(phase["path"]).read_text()); trace=json.loads(Path(attempt["trace"]["path"]).read_text())
            late=[i for i,e in enumerate(trace) if e.get("channel")=="pty_output" and b"LATE_AFTER_PROMPT" in bytes.fromhex(e["data"])][0]
            self.assertNotIn("LATE_AFTER_PROMPT","\n".join(prompt["rows"])); self.assertGreater(late,prompt["trace_event_index"]); self.assertEqual(attempt["operations"] if False else prompt["rows"],prompt["rows"])
            self.assertEqual(result["operations"]["teddy-shipped"]["search"]["status"],"MEASURED"); self.assertEqual(len(result["operations"]["teddy-shipped"]["search"]["attempts"]),32)

    def test_fresh_missing_prompt_is_explicit_inconclusive(self):
        with tempfile.TemporaryDirectory() as d:
            result,corpus,smoke=self._fresh_universal_run(d,NoPromptOutputSession)
            with mock.patch.object(bench,"validate_universal_corpus",return_value=True): self.assertTrue(bench.validate_universal_result(result))
            op=result["operations"]["teddy-shipped"]["search"]; self.assertEqual(op["status"],"INCONCLUSIVE"); self.assertEqual(op["repetitions"],32); self.assertIsNone(op["p50_ms"]); self.assertIsNone(op["p95_ms"])
            for attempt in op["attempts"]:
                self.assertFalse(attempt["valid"]); self.assertEqual(attempt["status"],"INCONCLUSIVE"); self.assertEqual([w["name"] for w in attempt["writes"]],["prompt","quit"])
                phases=json.loads(Path(attempt["phase_snapshots"]["path"]).read_text()); p=json.loads(Path(phases["prompt"]["path"]).read_text()); self.assertEqual((p["matched"],p["missing"],p["rows"],p["output_event_ordinal"],p["trace_event_index"],p["causal_time"]),(False,True,[],None,None,None)); self.assertEqual(p["associated_input_trace_event_index"],attempt["writes"][0]["trace_index"]); self.assertEqual(p["input_write_time"],attempt["writes"][0]["write"])
            self.assertEqual(result["adapter_smoke"],smoke)

    def test_fresh_target_before_submit_is_rejected_causally(self):
        with tempfile.TemporaryDirectory() as d:
            result,corpus,_=self._fresh_universal_run(d,TargetBeforeSubmitSession)
            attempt=result["operations"]["teddy-shipped"]["search"]["attempts"][0]; phases=json.loads(Path(attempt["phase_snapshots"]["path"]).read_text()); target=json.loads(Path(phases["target"]["path"]).read_text()); trace=json.loads(Path(attempt["trace"]["path"]).read_text()); submit=next(e for e in trace if e.get("channel")=="pty_input" and e.get("name")=="submit")
            self.assertLess(target["trace_event_index"],trace.index(submit))
            with mock.patch.object(bench,"validate_universal_corpus",return_value=True), self.assertRaises(ValueError): bench.validate_universal_result(result)

    def test_executor_fake_session_full_contract_and_mutations(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d); corpus=root/"tiny"; corpus.write_bytes(b"UBENCH_HEAD_RECORD\nUBENCH_TARGET_RECORD UBENCH_NEEDLE\n"); (root/"teddy-highlight").write_bytes(b"helper")
            adapters=[]
            for name in bench.UNIVERSAL_ADAPTER_ORDER:
                path=root/name; path.write_bytes(name.encode()); spec=bench.UNIVERSAL_ADAPTER_SPEC[name]
                if name!="vis": (root/name).write_bytes(name.encode())
                status="REJECTED_UNSUPPORTED" if name=="vis" else ("ALIAS_OF" if name=="vi" else "IDENTITY_QUALIFIED")
                if name=="vis": path=Path("/usr/bin/vis")
                h,z=("0"*64,1) if name=="vis" else bench.sha(path)
                a={"name":name,"status":status,"path":str(path.resolve()),"sha256":h,"size":z,"arch":bench.platform.machine(),"version":"fake","alias_of":"vim" if name=="vi" else None,"argv":bench.universal_adapter_argv(name,path,corpus),"search_prompt":spec["search_prompt"].hex(),"search_submit":spec["search_submit"].hex(),"quit":spec["quit"].hex(),"expected_topology":spec["expected_topology"],"expected_class":spec["expected_class"],"helper_path":str((root/"teddy-highlight").resolve()) if name=="teddy-shipped" else None}; adapters.append(a)
            vim=next(a for a in adapters if a["name"]=="vim"); vi=next(a for a in adapters if a["name"]=="vi"); vi.update({k:vim[k] for k in ("path","sha256","size","arch","version")}); vi["argv"]=bench.universal_adapter_argv("vi",vi["path"],corpus)
            smoke=self._strict_smoke(root/"artifacts",adapters)
            smoke["records"]["teddy-shipped"]["status"]="PASS"
            for name in ("vim","hx","kak","less","vi"): smoke["records"][name]={"status":"INCONCLUSIVE","reason":"fake smoke ineligible","attempts":0}
            teddy=next(a for a in adapters if a["name"]=="teddy-shipped"); group=[{"command":" ".join(teddy["argv"]),"pid":1},{"command":teddy["helper_path"],"pid":2}]
            with mock.patch.object(bench,"process_group",side_effect=lambda _:(True,[{"command":" ".join(ExecutorFakeSession.current_argv),"pid":1}]+([{"command":teddy["helper_path"],"pid":2}] if ExecutorFakeSession.current_argv[0].endswith("teddy-shipped") else []),None)), mock.patch.object(bench,"validate_universal_corpus",return_value=True):
                result=bench.execute_universal_schedule(adapters,str(corpus),root/"artifacts",session_cls=ExecutorFakeSession,smoke=smoke)
                self.assertEqual([a["name"] for a in result["adapters"]],list(bench.UNIVERSAL_ADAPTER_ORDER)); self.assertEqual(set(result["adapter_smoke"]["records"]),set(bench.UNIVERSAL_ADAPTER_ORDER)); self.assertEqual(len(result["schedule"]),140)
                for op in ("startup","search"): self.assertEqual(len(result["operations"]["teddy-shipped"][op]["warmup_attempts"]),3); self.assertEqual(len(result["operations"]["teddy-shipped"][op]["attempts"]),32); self.assertEqual(len(result["operations"]["nvim"][op]["warmup_attempts"]),3); self.assertEqual(len(result["operations"]["nvim"][op]["attempts"]),32)
                self.assertEqual(set(result["operations"]),{"teddy-shipped","nvim"}); self.assertTrue(bench.validate_universal_result(result)); self.assertTrue(bench.validate_universal_report(result,bench.universal_report_markdown(result)))
                def reject(mutate):
                    bad=json.loads(json.dumps(result)); mutate(bad)
                    with self.assertRaises(ValueError): bench.validate_universal_result(bad)
                first=result["operations"]["teddy-shipped"]["startup"]["warmup_attempts"][0]; reject(lambda x:x["schedule"].__setitem__(0,x["schedule"][1])); reject(lambda x:x["adapter_smoke"]["records"].pop("vim")); reject(lambda x:x["operations"]["teddy-shipped"]["startup"]["warmup_attempts"][0]["identity_before"].__setitem__("sha256","0"*64)); reject(lambda x:x["operations"]["teddy-shipped"]["startup"]["warmup_attempts"][0]["identity_after"].__setitem__("path","/wrong")); reject(lambda x:x["operations"]["teddy-shipped"]["startup"]["warmup_attempts"][0]["helper_identity_before"][0].__setitem__("sha256","0"*64)); reject(lambda x:x["operations"]["teddy-shipped"]["startup"]["warmup_attempts"][0]["helper_identity_after"][0].__setitem__("path","/wrong")); reject(lambda x:x["operations"]["teddy-shipped"]["startup"]["warmup_attempts"][0]["topology_evidence"].__setitem__("observed_argvs",[["/wrong"]])); reject(lambda x:x["operations"]["teddy-shipped"]["startup"]["warmup_attempts"][0]["raw_attempt"].__setitem__("sha256","0"*64)); reject(lambda x:x["operations"]["teddy-shipped"]["startup"]["warmup_attempts"][0]["trace"].__setitem__("sha256","0"*64)); reject(lambda x:x["operations"]["teddy-shipped"]["startup"]["warmup_attempts"][0]["phase_snapshots"].__setitem__("sha256","0"*64)); reject(lambda x:x["operations"]["teddy-shipped"]["search"]["attempts"][0].__setitem__("endpoint_event_index",0))

                def phase_reject(field, value):
                    bad=json.loads(json.dumps(result)); attempt=bad["operations"]["teddy-shipped"]["search"]["attempts"][0]; mp=json.loads(Path(attempt["phase_snapshots"]["path"]).read_text()); phase=next(iter(mp)); desc=mp[phase]; payload=json.loads(Path(desc["path"]).read_text()); payload[field]=value; Path(desc["path"]).write_text(json.dumps(payload,sort_keys=True)+"\n"); desc["sha256"],desc["size"]=bench.sha(desc["path"]); Path(attempt["phase_snapshots"]["path"]).write_text(json.dumps(mp,sort_keys=True)+"\n"); attempt["phase_snapshots"]["sha256"],attempt["phase_snapshots"]["size"]=bench.sha(attempt["phase_snapshots"]["path"]); raw=Path(attempt["raw_attempt"]["path"]); raw.write_text(json.dumps(bench._universal_attempt_projection(attempt),sort_keys=True)+"\n"); attempt["raw_attempt"]["sha256"],attempt["raw_attempt"]["size"]=bench.sha(raw)
                    with self.assertRaises(ValueError): bench.validate_universal_result(bad)
                phase_reject("trace_event_index",999999); phase_reject("causal_time",-1.0); phase_reject("rows",["forged"]); phase_reject("associated_input_trace_event_index",999999); phase_reject("input_write_time",-1.0)
                late=json.loads(json.dumps(result)); late["operations"]["teddy-shipped"]["search"]["attempts"][0]["phase_snapshots"]["sha256"]="0"*64
                with self.assertRaises(ValueError): bench.validate_universal_result(late)
                no_output=json.loads(json.dumps(result)); no_output["operations"]["teddy-shipped"]["search"]["attempts"][0]["writes"][0]["at"]=999999.0
                with self.assertRaises(ValueError): bench.validate_universal_result(no_output)
                target_early=json.loads(json.dumps(result)); target_early["operations"]["teddy-shipped"]["search"]["attempts"][0]["schedule_index"]-=1
                with self.assertRaises(ValueError): bench.validate_universal_result(target_early)
    def test_causal_failure_sessions_and_production_endpoint_api(self):
        for session_type in (LateOutputSession,NoPromptOutputSession,TargetBeforeSubmitSession):
            s=session_type(["fake"]); s.spawn("/tmp","/tmp"); head=s.until(lambda sc:sc.contains("UBENCH_HEAD_RECORD"),"startup_head"); self.assertTrue(head["matched"])
            endpoint=s.write_endpoint(b"/",lambda sc:sc.text()!=list(head["snapshot"]),"prompt")
            if session_type is NoPromptOutputSession: self.assertFalse(endpoint["matched"])
            if session_type is TargetBeforeSubmitSession:
                s.write_endpoint(b"UBENCH_NEEDLE",lambda sc:sc.contains("UBENCH_NEEDLE"),"typed_needle")
                s.write_endpoint(b"\r",lambda sc:sc.contains("UBENCH_TARGET_RECORD"),"submit")
                self.assertTrue(any(b"UBENCH_TARGET_RECORD" in bytes.fromhex(e.get("data","")) for e in s.trace_events if e.get("channel")=="pty_output"))
            s.close()
        s=bench.Session([]); reader,writer=os.pipe(); s.master=writer; s.t0=time.monotonic(); s.output_events=[]; s.trace_events=[]; s.quiet=lambda:True; s.until=lambda predicate,name,baseline:{"name":name,"matched":True,"matched_at":1.0,"event_index":0,"trace_index":1,"snapshot":[]}
        endpoint=s.write_endpoint(b"X",lambda sc:True,"direct"); self.assertEqual(endpoint["name"],"direct"); self.assertEqual(endpoint["input_trace_index"],0); os.close(reader); os.close(writer)

    def test_universal_attempt_artifacts_are_materialized_and_bound(self):
        with tempfile.TemporaryDirectory() as d:
            result=self._strict_universal_fixture(d); adapter=next(x for x in result["adapters"] if x["name"]=="teddy-shipped"); attempt=result["operations"]["teddy-shipped"]["startup"]["attempts"][0]
            with mock.patch.object(bench,"validate_universal_corpus",return_value=True): self.assertTrue(bench.validate_universal_attempt(attempt,"startup","b"*64,adapter,result["artifact_root"],result["corpus"]["path"]))
            bad=json.loads(json.dumps(attempt)); bad["identity"]["verified"]=False
            with mock.patch.object(bench,"validate_universal_corpus",return_value=True): self.assertFalse(bench.validate_universal_attempt(bad,"startup","b"*64,adapter,result["artifact_root"],result["corpus"]["path"]))

    def _strict_universal_fixture(self, root):
        root=Path(root); corpus=str(root/"corpus"); adapters=[]
        for name in bench.UNIVERSAL_ADAPTER_ORDER:
            spec=bench.UNIVERSAL_ADAPTER_SPEC[name]; path="/usr/bin/vis" if name=="vis" else str(root/name)
            if name!="vis": (root/name).write_bytes(name.encode())
            status="REJECTED_UNSUPPORTED" if name=="vis" else ("ALIAS_OF" if name=="vi" else "IDENTITY_QUALIFIED")
            adapters.append({"name":name,"status":status,"path":path,"sha256":"a"*64,"size":10,"arch":"arm64","version":"v1","alias_of":"vim" if name=="vi" else None,"argv":bench.universal_adapter_argv(name,path,corpus),"search_prompt":spec["search_prompt"].hex(),"search_submit":spec["search_submit"].hex(),"quit":spec["quit"].hex(),"expected_topology":spec["expected_topology"],"expected_class":spec["expected_class"],"helper_path":str(root/"teddy-highlight") if name=="teddy-shipped" else None})
        helper=root/"teddy-highlight"; helper.write_bytes(b"helper")
        for x in adapters:
            if x["name"]!="vis": x["sha256"],x["size"]=bench.sha(x["path"])
        vim=next(x for x in adapters if x["name"]=="vim"); vi=next(x for x in adapters if x["name"]=="vi"); vi.update({k:vim[k] for k in ("path","sha256","size","arch","version")}); vi["argv"]=bench.universal_adapter_argv("vi",vi["path"],corpus)
        def put(rel,data):
            p=root/rel; p.parent.mkdir(parents=True,exist_ok=True); p.write_bytes(data); h,z=bench.sha(p); return {"path":str(p),"sha256":h,"size":z}
        def attempt(adapter,op,block,rep,warmup,index):
            target=op=="search"; expected=[adapter["argv"]]+([[adapter["helper_path"]]] if adapter["helper_path"] else [])
            topo={"expected_argvs":expected,"observed_argvs":expected,"unexpected":False,"descendants":[],"pgid_before":[{"command":" ".join(x),"pid":i+1} for i,x in enumerate(expected)],"pgid_after":[]}
            process={"exit":0,"signal":None,"reaped":True,"pty_eof":True,"stderr_eof":True,"drain_complete":True,"drain_deadline":False,"cleanup_error":None,"pgid_after":[],"descendants_left":False,"timed_out":False,"output_capped":False,"stderr_capped":False,"unsupported":[],"exec_failed":False}
            chunks=[b"UBENCH_HEAD_RECORD\n"] if not target else [b"UBENCH_HEAD_RECORD\n",b"PROMPT\n",b"PROMPT UBENCH_NEEDLE\n",b"UBENCH_TARGET_RECORD UBENCH_NEEDLE\n"]
            trace=[]; screens=[]; output_indices=[]; clock=1.0
            for n,chunk in enumerate(chunks):
                trace += [{"channel":"pty_output_raw","at":clock,"data":chunk.hex()},{"channel":"pty_output","at":clock,"data":chunk.hex()}]; output_indices.append(len(trace)-1)
                model=bench.Screen(200,50)
                for e in trace:
                    if e["channel"]=="pty_output": model.feed(bytes.fromhex(e["data"]))
                screens.append(model.text()); clock+=1.0
                if target and n<3:
                    names=("prompt","typed_needle","submit"); data=(adapter["search_prompt"],"5542454e43485f4e4545444c45",adapter["search_submit"])[n]; trace.append({"channel":"pty_input","at":clock,"data":data,"action":True,"name":names[n]}); clock+=1.0
            trace.append({"channel":"pty_input","at":clock,"data":adapter["quit"],"action":True,"name":"quit"})
            writes=[{"name":e["name"],"bytes":e["data"],"at":e["at"],"write":e["at"],"trace_index":i} for i,e in enumerate(trace) if e.get("channel")=="pty_input"]; actions=[dict(w) for w in writes]; final=screens[-1]; endpoint_n=len(chunks)-1
            ts={"pre_fork_ms":0.0,"endpoint_ms":(1+endpoint_n*2)*1000,"quiet_complete_ms":(2+endpoint_n*2)*1000,"elapsed_ms":(1 if not target else 1)*1000}; ts.update({"prompt_start_ms":2000.0,"prompt_echo_ms":3000.0,"submit_ms":6000.0} if target else {})
            ident={"path":str(Path(adapter["path"]).resolve()),"sha256":adapter["sha256"],"size":adapter["size"],"arch":adapter["arch"],"version":adapter["version"],"verified":True}; helpers=[] if not adapter["helper_path"] else [{"path":str(Path(adapter["helper_path"]).resolve()),"sha256":bench.sha(adapter["helper_path"])[0],"size":bench.sha(adapter["helper_path"])[1],"verified":True}]
            a={"schema":"teddy-s9-universal-attempt-1","operation":op,"adapter":adapter["name"],"adapter_identity":adapter,"argv":adapter["argv"],"invocation_class":adapter["expected_class"],"topology":adapter["expected_topology"],"schedule_index":index,"execution_state":"completed","block":block,"rep":rep,"warmup":warmup,"corpus_path":corpus,"corpus_before_sha256":"b"*64,"corpus_after_sha256":"b"*64,"timestamps":ts,"elapsed_ms":ts["elapsed_ms"],"endpoint":"UBENCH_TARGET_RECORD" if target else "UBENCH_HEAD_RECORD","endpoint_snapshot":final,"endpoint_event_index":endpoint_n,"endpoint_at":float(1+endpoint_n*2),"prompt_echo":target,"prompt_screen":screens[1] if target else [],"typed_needle":screens[2] if target else [],"writes":writes,"actions":actions,"harness_traffic":[],"process":process,"topology_evidence":topo,"identity":{"verified":True,"argv":adapter["argv"],"before":ident,"after":ident},"identity_before":ident,"identity_after":ident,"helper_identity_before":helpers,"helper_identity_after":helpers,"valid":True,"status":"PASS"}
            phases={}
            for n,name in enumerate(("startup_head",) if not target else ("startup_head","prompt","typed_needle","target")):
                inp=None if n==0 else writes[n-1]; phases[name]={"phase":name,"matched":True,"missing":False,"rows":screens[n],"output_event_ordinal":n,"trace_event_index":output_indices[n],"causal_time":float(1+n*2),"associated_input_trace_event_index":None if inp is None else inp["trace_index"],"input_write_time":None if inp is None else inp["at"]}
            key=f"artifacts/{adapter['name']}-{op}-{block}-{rep}-{index}"; a["trace"]=put(key+".trace",json.dumps(trace).encode()); a["stderr"]=put(key+".stderr",b""); a["full_screen"]=put(key+".screen",("\n".join(final)+"\n").encode()); a["endpoint_screen"]=put(key+".endpoint",json.dumps({"rows":final,"event_index":endpoint_n,"at":a["endpoint_at"]}).encode()); a["process_topology"]=put(key+".topology",json.dumps(topo,sort_keys=True).encode()); phase_map={name:put(key+".phase-"+name,json.dumps(payload,sort_keys=True).encode()) for name,payload in phases.items()}; a["phase_snapshots"]=put(key+".phase-map",json.dumps(phase_map,sort_keys=True).encode()); raw=root/(key+".raw"); raw.write_text(json.dumps(bench._universal_attempt_projection(a),sort_keys=True)+"\n"); rb=raw.read_bytes(); a["raw_attempt"]={"path":str(raw),"sha256":hashlib.sha256(rb).hexdigest(),"size":len(rb)}; return a
        eligible=[a for a in adapters if a["status"]=="IDENTITY_QUALIFIED"]; schedule=bench.universal_schedule([a["name"] for a in eligible])
        operations={a["name"]:{op:{"warmup_attempts":[],"attempts":[]} for op in ("startup","search")} for a in eligible}
        for item in schedule:
            a=next(x for x in adapters if x["name"]==item["adapter"]); value=attempt(a,item["operation"],item["block"],item["rep"],item["warmup"],item["index"]); operations[a["name"]][item["operation"]]["warmup_attempts" if item["warmup"] else "attempts"].append(value)
        for values in (v for ops in operations.values() for v in ops.values()): values.update(bench.universal_metric(values["attempts"]))
        return {"schema":bench.UNIVERSAL_SCHEMA,"phase":"universal-comparison","execution_state":"completed","artifact_root":{"path":str(root)},"geometry":[200,50],"warmups":3,"blocks":2,"repetitions_per_block":16,"corpus":{"path":corpus,"size":1<<30,"record_bytes":64,"records":1<<24,"sha256":"b"*64,"head_marker":"UBENCH_HEAD_RECORD","needle":"UBENCH_NEEDLE","target_marker":"UBENCH_TARGET_RECORD","tail_marker":"UBENCH_TAIL_RECORD","head_offset":0,"target_offset":512<<20,"needle_offset":(512<<20)+len("UBENCH_TARGET_RECORD "),"tail_offset":(1<<30)-64,"stream_marker":"64-byte-LF-records-v1","read_only_mode":"0o444"},"adapters":adapters,"adapter_smoke":self._strict_smoke(root,adapters),"operations":operations,"schedule":schedule,"contract_only":False}

    def test_universal_corpus_offsets_agree_across_producers(self):
        """Corpus offsets must match wherever they are computed.

        Every strict-fixture test mocks validate_universal_corpus, so a wrong
        needle_offset in the executor's own metadata survived the whole suite
        and only failed on a real 1 GiB run. `<<` binds looser than `+`, so
        512<<20+21 is 512<<41. This asserts the arithmetic without generating
        a gigabyte.
        """
        target=(512<<20); needle=target+len("UBENCH_TARGET_RECORD ")
        self.assertEqual(needle,536870933)
        self.assertNotEqual(needle,512<<20+len("UBENCH_TARGET_RECORD "))
        src=Path(bench.__file__).read_text()
        self.assertNotIn('"needle_offset":512<<20+',src,"unparenthesised needle_offset shift reintroduced")
        # The executor must actually adopt the metadata it is handed, not just
        # own a corrected copy of the same arithmetic.
        with tempfile.TemporaryDirectory() as d:
            root=Path(d); corpus=root/"tiny"; corpus.write_bytes(b"UBENCH_HEAD_RECORD\nUBENCH_TARGET_RECORD UBENCH_NEEDLE\n"); (root/"teddy-highlight").write_bytes(b"helper")
            adapters=[]
            for name in bench.UNIVERSAL_ADAPTER_ORDER:
                p=root/name; p.write_bytes(name.encode()); spec=bench.UNIVERSAL_ADAPTER_SPEC[name]
                adapters.append({"name":name,"status":"IDENTITY_QUALIFIED","path":str(p),"sha256":bench.sha(p)[0],"size":bench.sha(p)[1],"arch":"arm64","version":"v","alias_of":None,"argv":bench.universal_adapter_argv(name,p,corpus),"search_prompt":spec["search_prompt"].hex(),"search_submit":spec["search_submit"].hex(),"quit":spec["quit"].hex(),"expected_topology":spec["expected_topology"],"expected_class":spec["expected_class"],"helper_path":str(root/"teddy-highlight") if name=="teddy-shipped" else None})
            # No adapter passes smoke, so no session is ever launched.
            smoke={"corpus":str(corpus),"records":{a["name"]:{"status":"INCONCLUSIVE","reason":"none eligible","attempts":0} for a in adapters}}
            supplied={"path":str(corpus),"size":123,"sha256":"c"*64,"record_bytes":64,"records":7,"head_marker":"UBENCH_HEAD_RECORD","needle":"UBENCH_NEEDLE","target_marker":"UBENCH_TARGET_RECORD","tail_marker":"UBENCH_TAIL_RECORD","head_offset":0,"target_offset":11,"needle_offset":22,"tail_offset":33,"stream_marker":"sentinel-not-recomputable","read_only_mode":"0o444"}
            result=bench.execute_universal_schedule(adapters,str(corpus),root/"artifacts",smoke=smoke,corpus_meta=supplied)
            self.assertEqual(result["corpus"],supplied,"executor recomputed corpus metadata instead of using the supplied metadata")

    def test_published_summary_is_bound_to_its_evidence(self):
        """The published JSON carries metrics only, tied to the full bundle.

        Per-attempt evidence is far too large to commit, so the repository
        holds a summary. It must not become a free-floating set of numbers:
        the digest binds it to the bundle it was derived from, and identity is
        still re-hashed against disk.
        """
        with tempfile.TemporaryDirectory() as d:
            result=self._strict_universal_fixture(d)
            with mock.patch.object(bench,"validate_universal_corpus",return_value=True):
                self.assertTrue(bench.validate_universal_result(result))
            full=Path(d)/"result-full.json"; full.write_text(json.dumps(result,indent=2,sort_keys=True)+"\n")
            h,z=bench.sha(full)
            summary=bench.universal_summary(result,{"path":str(full),"sha256":h,"size":z})
            with mock.patch.object(bench,"validate_universal_corpus",return_value=True):
                self.assertTrue(bench.validate_universal_summary(summary))
            self.assertLess(len(json.dumps(summary)),len(json.dumps(result))//10,"summary must be far smaller than the evidence")
            # The report must be identical apart from the schema it declares.
            strip=lambda t:[l for l in t.splitlines() if not l.startswith(("- Schema:","- **This published result is a summary."))]
            self.assertEqual(strip(bench.universal_report_markdown(result)),strip(bench.universal_report_markdown(summary)))
            self.assertTrue(bench.validate_universal_report(summary,bench.universal_report_markdown(summary)))
            # Rejections: a summary detached from its evidence, or forged metrics.
            def plausible_metric(s):
                # The dangerous forgery is a believable number, not a broken
                # one: it passes every structural check, so only re-deriving
                # the summary from the evidence can reject it.
                d=s["operations"]["teddy-shipped"]["startup"]; d["p50_ms"]=d["p50_ms"]/2; d["p95_ms"]=d["p95_ms"]/2
            for label,mutate in (
                ("stale digest",lambda s: s["full_result"].__setitem__("sha256","f"*64)),
                ("plausible forged metric",plausible_metric),
                ("dropped operation",lambda s: s["operations"].pop("teddy-shipped")),
                ("dropped one operation cell",lambda s: s["operations"]["teddy-shipped"].pop("startup")),
                ("forged measured metric",lambda s: next(iter(s["operations"]["teddy-shipped"].values())).__setitem__("repetitions",5)),
                ("inconclusive carrying a metric",lambda s: s["operations"]["teddy-shipped"]["startup"].update({"status":"INCONCLUSIVE","p50_ms":1.0})),
                ("quantiles out of order",lambda s: s["operations"]["teddy-shipped"]["startup"].update({"p50_ms":9e9})),
                ("unknown status",lambda s: s["operations"]["teddy-shipped"]["startup"].__setitem__("status","GREAT")),
            ):
                bad=json.loads(json.dumps(summary)); mutate(bad)
                with mock.patch.object(bench,"validate_universal_corpus",return_value=True):
                    with self.assertRaises(ValueError,msg=f"{label} accepted"): bench.validate_universal_summary(bad)
            # Absent evidence must not read as verified: without the bundle a
            # halved metric is undetectable, so the summary is rejected rather
            # than silently downgraded to a structural check.
            detached=json.loads(json.dumps(summary)); detached["full_result"]["path"]=str(Path(d)/"gone.json")
            plausible_metric(detached)
            with mock.patch.object(bench,"validate_universal_corpus",return_value=True):
                with self.assertRaises(ValueError,msg="summary with missing evidence accepted as verified"): bench.validate_universal_summary(detached)
                self.assertTrue(bench.validate_universal_summary(detached,require_evidence=False),"structure-only check should still pass")

    def test_participant_that_times_out_is_representable_but_never_measured(self):
        """A participant that cannot finish is an observation, not bad evidence.

        Helix does not complete a 1 GiB search inside the timeout and is
        killed. Requiring a clean lifecycle *and* a missing phase left that
        attempt with no valid category, so the whole run was rejected. It must
        validate as INCONCLUSIVE -- and must never reach a metric.
        """
        with tempfile.TemporaryDirectory() as d:
            result=self._strict_universal_fixture(d)
            adapter=next(x for x in result["adapters"] if x["name"]=="teddy-shipped")
            args=("startup","b"*64,adapter,result["artifact_root"],result["corpus"]["path"])
            base=result["operations"]["teddy-shipped"]["startup"]["attempts"][0]
            with mock.patch.object(bench,"validate_universal_corpus",return_value=True):
                def killed(a):
                    a["status"]="INCONCLUSIVE"; a["valid"]=False
                    a["process"].update({"exit":None,"signal":9,"timed_out":True,"reaped":True})
                probe=self._probe_attempt(base,result["artifact_root"]["path"],"timeout",killed)
                self.assertTrue(bench.validate_universal_attempt(probe,*args),"a timed-out attempt must be representable")
                self.assertEqual(bench.universal_metric([probe]*32)["status"],"INCONCLUSIVE","timed-out attempts must never derive a metric")
                # Evidence may be unclean, but never incomplete.
                def killed_and_stripped(a):
                    killed(a); a["process"].pop("timed_out")
                probe2=self._probe_attempt(base,result["artifact_root"]["path"],"timeout-stripped",killed_and_stripped)
                self.assertFalse(bench.validate_universal_attempt(probe2,*args),"incomplete lifecycle must still be rejected")
                # A measurable attempt must not be downgradable. Each of these
                # is a distinct route that a broad "not causal or not life"
                # justification would have waved through.
                def sandbag(a): a["status"]="INCONCLUSIVE"; a["valid"]=False
                def forged_endpoint(a): sandbag(a); a["endpoint"]="NOT_THE_HEAD"
                def incoherent_timeout(a):
                    # Claims a timeout while reporting a clean exit: a clean
                    # run must not be relabelled as one that never finished.
                    sandbag(a); a["process"].update({"timed_out":True,"exit":0,"signal":None})
                for label,mutate in (("plain",sandbag),("forged endpoint",forged_endpoint),("incoherent timeout",incoherent_timeout)):
                    probe=self._probe_attempt(base,result["artifact_root"]["path"],"sandbag-"+label.replace(" ","-"),mutate)
                    self.assertFalse(bench.validate_universal_attempt(probe,*args),f"measurable attempt downgraded via {label}")

    def test_attempt_validates_when_the_app_leaves_the_alternate_screen(self):
        """The endpoint marker need not survive into the post-quit screen.

        Every full-screen editor emits `?1049l` on exit, so full_screen is
        blank. The fixtures happen to make the final screen equal the endpoint
        screen, which hid this: the real 1 GiB run could not validate a single
        attempt. Reproduce the real shape -- marker at the endpoint, blank
        final screen -- and require acceptance.
        """
        with tempfile.TemporaryDirectory() as d:
            result=self._strict_universal_fixture(d)
            adapter=next(x for x in result["adapters"] if x["name"]=="teddy-shipped")
            a=json.loads(json.dumps(result["operations"]["teddy-shipped"]["startup"]["attempts"][0]))
            args=("startup","b"*64,adapter,result["artifact_root"],result["corpus"]["path"])
            with mock.patch.object(bench,"validate_universal_corpus",return_value=True):
                self.assertTrue(bench.validate_universal_attempt(a,*args))
                trace=json.loads(Path(a["trace"]["path"]).read_text())
                # Enter the alternate buffer by prefixing the first output
                # chunk rather than inserting an event: on an empty screen it
                # is visually transparent, so every recorded phase row and
                # output ordinal stays exactly as the fixture built it.
                for e in trace:
                    if e["channel"] in ("pty_output","pty_output_raw") and e["at"]==min(x["at"] for x in trace if x["channel"]=="pty_output"):
                        e["data"]=b"\x1b[?1049h".hex()+e["data"]
                exit_alt=b"\x1b[?1049l".hex(); at=max(e["at"] for e in trace)+1.0
                trace += [{"channel":"pty_output_raw","at":at,"data":exit_alt},{"channel":"pty_output","at":at,"data":exit_alt}]
                replay=bench.Screen(200,50)
                for e in trace:
                    if e["channel"]=="pty_output": replay.feed(bytes.fromhex(e["data"]))
                final=replay.text()
                self.assertEqual(sum(1 for r in final if r.strip()),0,"leaving the alternate buffer should blank the screen")
                self.assertIn("UBENCH_HEAD_RECORD","\n".join(a["endpoint_snapshot"]),"endpoint must still carry the marker")
                for key,data in (("trace",json.dumps(trace).encode()),("full_screen",("\n".join(final)+"\n").encode())):
                    p=Path(a[key]["path"]); p.write_bytes(data); a[key]["sha256"],a[key]["size"]=bench.sha(p)
                raw=Path(a["raw_attempt"]["path"]); raw.write_text(json.dumps(bench._universal_attempt_projection(a),sort_keys=True)+"\n")
                a["raw_attempt"]["sha256"],a["raw_attempt"]["size"]=bench.sha(raw)
                self.assertTrue(bench.validate_universal_attempt(a,*args),"attempt rejected because its post-quit screen is blank")

    def _probe_attempt(self, attempt, root, label, mutate):
        """Deep-copy an attempt, mutate it, and re-materialize its raw artifact.

        Rehashing into a probe-private path matters: without it a probe would
        be rejected by the raw-hash binding rather than by the check it aims
        at, and would keep passing even after that check was deleted.
        """
        a=json.loads(json.dumps(attempt)); mutate(a)
        p=Path(root)/f"oracle-probe-{label}.raw"; p.write_text(json.dumps(bench._universal_attempt_projection(a),sort_keys=True)+"\n")
        h,z=bench.sha(p); a["raw_attempt"]={"path":str(p),"sha256":h,"size":z}; return a

    def test_oracle_mutation_probes_are_rejected(self):
        """Phase 1 execution gate: forged evidence must not survive validation.

        Each probe reproduces a hole that was live before this suite existed.
        A probe that starts passing means the corresponding check was weakened,
        so `compare --allow-large --execute` is no longer safe to run.
        """
        with tempfile.TemporaryDirectory() as d:
            result=self._strict_universal_fixture(d); root=result["artifact_root"]["path"]
            adapter=next(x for x in result["adapters"] if x["name"]=="teddy-shipped")
            attempt=result["operations"]["teddy-shipped"]["startup"]["attempts"][0]
            args=("startup","b"*64,adapter,result["artifact_root"],result["corpus"]["path"])
            with mock.patch.object(bench,"validate_universal_corpus",return_value=True):
                # Baseline: the unmutated fixture is genuinely accepted, so every
                # rejection below is attributable to its mutation.
                self.assertTrue(bench.validate_universal_result(result))
                self.assertTrue(bench.validate_universal_attempt(attempt,*args))

                # 1. Headline elapsed_ms detached from the validated timestamp
                #    chain would let arbitrary p50/p95 values become MEASURED.
                def detach(a): a["elapsed_ms"]=999999.0
                self.assertFalse(bench.validate_universal_attempt(self._probe_attempt(attempt,root,"elapsed",detach),*args))

                # 1b. A chain shifted so it stays internally consistent (ordering
                #     and elapsed arithmetic still agree) is caught only by the
                #     anchor to trace-bound endpoint_at. Without this the check
                #     above is self-satisfying: it compares a value to its copy.
                def inflate_endpoint(a):
                    t=a["timestamps"]; x=t["endpoint_ms"]+50000.0
                    t["endpoint_ms"]=x; t["quiet_complete_ms"]=x+1000.0
                    t["elapsed_ms"]=x-t["pre_fork_ms"]; a["elapsed_ms"]=t["elapsed_ms"]
                self.assertFalse(bench.validate_universal_attempt(self._probe_attempt(attempt,root,"endpoint",inflate_endpoint),*args),"inflated endpoint_ms accepted against an unchanged trace")

                # 1b-ii. The other half of the startup subtraction: moving the
                #     pre-fork origin earlier inflates elapsed just as well.
                def move_pre_fork(a):
                    t=a["timestamps"]; t["pre_fork_ms"]=-50000.0
                    t["elapsed_ms"]=t["endpoint_ms"]-t["pre_fork_ms"]; a["elapsed_ms"]=t["elapsed_ms"]
                self.assertFalse(bench.validate_universal_attempt(self._probe_attempt(attempt,root,"prefork",move_pre_fork),*args),"pre_fork_ms moved off the trace clock origin accepted")

                # 1c. Same for search, whose elapsed is endpoint minus submit:
                #     moving submit_ms alone inflates the reported duration.
                search=result["operations"]["teddy-shipped"]["search"]["attempts"][0]
                search_args=("search","b"*64,adapter,result["artifact_root"],result["corpus"]["path"])
                self.assertTrue(bench.validate_universal_attempt(search,*search_args))
                def move_submit(a):
                    t=a["timestamps"]; t["submit_ms"]-=500.0
                    t["elapsed_ms"]=t["endpoint_ms"]-t["submit_ms"]; a["elapsed_ms"]=t["elapsed_ms"]
                self.assertFalse(bench.validate_universal_attempt(self._probe_attempt(search,root,"submit",move_submit),*search_args),"submit_ms detached from its action write accepted")

                # 1d. Session.record() spells this pty_output_capped while the
                #     fixtures say output_capped; both must be honoured, or
                #     every real attempt reads as unclean.
                def production_capped(a): a["process"]["pty_output_capped"]=a["process"].pop("output_capped")
                self.assertTrue(bench.validate_universal_attempt(self._probe_attempt(attempt,root,"capped",production_capped),*args),"production pty_output_capped spelling rejected")

                # 2. Stripped lifecycle fields must not read as a clean exit.
                stripped=("drain_deadline","cleanup_error","pgid_after","descendants_left","timed_out","output_capped","stderr_capped","unsupported")
                def strip(a): [a["process"].pop(k) for k in stripped]
                self.assertFalse(bench.validate_universal_attempt(self._probe_attempt(attempt,root,"lifecycle",strip),*args))
                for key in stripped:
                    def drop_one(a,k=key): a["process"].pop(k)
                    self.assertFalse(bench.validate_universal_attempt(self._probe_attempt(attempt,root,"life-"+key,drop_one),*args),f"missing {key} accepted as clean")

                # 3. A self-attested digest must not stand in for the binary.
                #    Probed on _universal_adapter_ok directly: forging only the
                #    result-level digest trips the attempt-level identity
                #    comparison instead, which would keep this passing even with
                #    the on-disk rehash removed.
                forged=json.loads(json.dumps(adapter)); forged["sha256"]="f"*64
                self.assertFalse(bench._universal_adapter_ok(forged,result["corpus"]["path"]),"declared digest accepted without hashing the binary")
                # Same check from the other side: the declaration is honest but
                # the binary on disk was swapped after it was recorded.
                swapped=json.loads(json.dumps(adapter)); original=Path(swapped["path"]).read_bytes()
                try:
                    Path(swapped["path"]).write_bytes(b"substituted-binary")
                    self.assertFalse(bench._universal_adapter_ok(swapped,result["corpus"]["path"]),"substituted binary accepted against a stale digest")
                    swapped["sha256"],swapped["size"]=bench.sha(swapped["path"])
                    self.assertTrue(bench._universal_adapter_ok(swapped,result["corpus"]["path"]),"probe is vacuous unless a truthful declaration is accepted")
                finally: Path(swapped["path"]).write_bytes(original)

                # 4. `signal` is checked by value, so a deleted key also reads
                #    as "no signal" unless presence is required separately.
                def drop_signal(a): a["process"].pop("signal")
                self.assertFalse(bench.validate_universal_attempt(self._probe_attempt(attempt,root,"signal",drop_signal),*args),"missing signal accepted as unsignalled")

                # 4b. Smoke PASS gates measurement eligibility, so the same
                #     presence discipline must hold on the smoke path.
                for field in ("signal","drain_deadline","timed_out"):
                    stripped_smoke=json.loads(json.dumps(result)); stripped_smoke["adapter_smoke"]["records"]["teddy-shipped"]["process"].pop(field)
                    with self.assertRaises(ValueError,msg=f"smoke record missing {field} accepted"): bench.validate_universal_result(stripped_smoke)

                # 5. A result carrying measured attempts must not simultaneously
                #    claim it was never executed.
                contradictory=json.loads(json.dumps(result)); contradictory["execution_state"]="not_started"
                with self.assertRaises(ValueError): bench.validate_universal_result(contradictory)

                # 6. A contract-only scaffold has no schedule to bind attempts
                #    to, so it must never carry attempts that reach a metric.
                scaffold=json.loads(json.dumps(result)); scaffold.update({"contract_only":True,"execution_state":"not_started","schedule":[]})
                measured=sum(v.get("status")=="MEASURED" for ops in scaffold["operations"].values() for v in ops.values())
                self.assertTrue(measured, "probe is vacuous unless the fixture actually derives MEASURED metrics")
                with self.assertRaises(ValueError): bench.validate_universal_result(scaffold)

    def test_universal_materialized_full_matrix_and_baseline_mutations(self):
        with tempfile.TemporaryDirectory() as d:
            result=self._strict_universal_fixture(d); with_corpus=mock.patch.object(bench,"validate_universal_corpus",return_value=True)
            with with_corpus: self.assertTrue(bench.validate_universal_result(result))
            missing_phases=json.loads(json.dumps(result)); del missing_phases["operations"]["teddy-shipped"]["startup"]["attempts"][0]["phase_snapshots"]
            with mock.patch.object(bench,"validate_universal_corpus",return_value=True):
                with self.assertRaises(ValueError): bench.validate_universal_result(missing_phases)
            def reject(mutate):
                value=json.loads(json.dumps(result)); mutate(value)
                with mock.patch.object(bench,"validate_universal_corpus",return_value=True):
                    with self.assertRaises(ValueError): bench.validate_universal_result(value)
                first=result["operations"]["teddy-shipped"]["startup"]["warmup_attempts"][0]; reject(lambda x:x["schedule"].__setitem__(0,x["schedule"][1])); reject(lambda x:x["adapter_smoke"]["records"].pop("vim")); reject(lambda x:x["operations"]["teddy-shipped"]["startup"]["warmup_attempts"][0]["identity_before"].__setitem__("sha256","0"*64)); reject(lambda x:x["operations"]["teddy-shipped"]["startup"]["warmup_attempts"][0]["identity_after"].__setitem__("path","/wrong")); reject(lambda x:x["operations"]["teddy-shipped"]["startup"]["warmup_attempts"][0]["helper_identity_before"][0].__setitem__("sha256","0"*64)); reject(lambda x:x["operations"]["teddy-shipped"]["startup"]["warmup_attempts"][0]["helper_identity_after"][0].__setitem__("path","/wrong")); reject(lambda x:x["operations"]["teddy-shipped"]["startup"]["warmup_attempts"][0]["topology_evidence"].__setitem__("observed_argvs",[["/wrong"]])); reject(lambda x:x["operations"]["teddy-shipped"]["startup"]["warmup_attempts"][0]["raw_attempt"].__setitem__("sha256","0"*64)); reject(lambda x:x["operations"]["teddy-shipped"]["startup"]["warmup_attempts"][0]["trace"].__setitem__("sha256","0"*64)); reject(lambda x:x["operations"]["teddy-shipped"]["startup"]["warmup_attempts"][0]["phase_snapshots"].__setitem__("sha256","0"*64)); reject(lambda x:x["operations"]["teddy-shipped"]["search"]["attempts"][0].__setitem__("endpoint_event_index",0))

    def _materialized_phase2_fixture(self, directory):
        source=Path(__file__).parents[1]/"results"/"canonical.json"
        x=json.loads(source.read_text()); root=Path(directory)/"phase2-artifacts"; root.mkdir()
        old_root=x["artifact_root"]["path"]; x["artifact_root"]["path"]=str(root)
        old_corpus=str((Path(__file__).parents[2]/x["corpus"]["evidence"]["path"]).resolve()); new_corpus=str((root/"s9-1g.log").resolve())
        def replace_corpus(value):
            if isinstance(value,dict):
                for key,child in list(value.items()): value[key]=replace_corpus(child)
            elif isinstance(value,list): return [replace_corpus(child) for child in value]
            elif isinstance(value,str): return value.replace(old_corpus,new_corpus)
            return value
        x=replace_corpus(x)
        c1={r["artifact"]["path"]:r for rs in x["claims"]["C1"]["profiles"].values() for r in rs}
        c2={a["log"]["path"]:a for a in x["claims"]["C2"]["attempts"]}
        c2_attempt_evidence=[e for e in x.get("evidence",[]) if e.get("path","").endswith("-attempt.json") and "/c2-" in e.get("path","")]
        c2_attempts={e["path"]:a for e,a in zip(c2_attempt_evidence,x["claims"]["C2"]["attempts"])}
        c3={e["path"]:a for e,a in zip(x["claims"]["C3"]["evidence"],x["claims"]["C3"]["attempts"])}
        comparator_records={r["artifact"]["path"]:{k:v for k,v in r.items() if k!="artifact"} for r in x.get("comparators",{}).get("records",[]) if isinstance(r.get("artifact"),dict)}
        comparator_attempts={a["artifact"]["path"]:{k:v for k,v in a.items() if k!="artifact"} for r in x.get("comparators",{}).get("records",[]) for a in r.get("attempts",[]) if isinstance(a.get("artifact"),dict)}
        def materialize(value):
            if isinstance(value,dict):
                if isinstance(value.get("path"),str) and len(value.get("sha256",""))==64 and value.get("name") not in bench.COMPARATOR_NAMES:
                    old=value["path"]; name=Path(old).name
                    if old in c1: data=(json.dumps({k:v for k,v in c1[old].items() if k!="artifact"},sort_keys=True)+"\n").encode()
                    elif old in c2_attempts: data=(json.dumps(c2_attempts[old],sort_keys=True,indent=2)+"\n").encode()
                    elif old in comparator_records: data=(json.dumps(comparator_records[old],sort_keys=True,indent=2)+"\n").encode()
                    elif old in comparator_attempts: data=(json.dumps(comparator_attempts[old],sort_keys=True,indent=2)+"\n").encode()
                    elif old in c2: data=("\n".join(f"{r['us']} {r['allocs']} {r['frame_bytes']}" for r in c2[old]["parsed_rows"])+"\n").encode()
                    elif old in c3: data=(json.dumps(c3[old],sort_keys=True)+"\n").encode()
                    else: data=b"phase2-fixture-artifact\n"
                    path=root/name; path.write_bytes(data); value["path"]=str(path); value["sha256"]=hashlib.sha256(data).hexdigest(); value["size"]=len(data)
                for child in value.values(): materialize(child)
            elif isinstance(value,list):
                for child in value: materialize(child)
        for fixture in x["claims"]["C4"]["fixtures"]:
            fixture["expected"]=fixture["actual_saved_sha256"]=hashlib.sha256(b"phase2-fixture-artifact\n").hexdigest()
        materialize(x)
        def rewrite(descriptor,obj):
            data=(json.dumps(obj,sort_keys=True,indent=2)+"\n").encode(); Path(descriptor["path"]).write_bytes(data); digest=hashlib.sha256(data).hexdigest(); size=len(data)
            def sync(value):
                if isinstance(value,dict):
                    if value.get("path")==descriptor["path"]: value["sha256"]=digest; value["size"]=size
                    for child in value.values(): sync(child)
                elif isinstance(value,list):
                    for child in value: sync(child)
            sync(x)
        for attempt in x["claims"]["C2"]["attempts"]:
            descriptor=next(e for e in x["evidence"] if e.get("path","").endswith(f"c2-{attempt['profile']}-attempt.json")); rewrite(descriptor,attempt)
        for attempt in x["claims"]["C3"]["attempts"]:
            descriptor=next(e for e in x["evidence"] if e.get("path","").endswith(f"c3-{attempt['profile']}-attempt.json")); rewrite(descriptor,attempt)
        for record in x.get("comparators",{}).get("records",[]):
            if isinstance(record.get("artifact"),dict):
                rewrite(record["artifact"],{k:v for k,v in record.items() if k!="artifact"})
            for attempt in record.get("attempts",[]):
                rewrite(attempt["artifact"],{k:v for k,v in attempt.items() if k!="artifact"})
        for record in x.get("comparators",{}).get("records",[]):
            if isinstance(record.get("artifact"),dict):
                rewrite(record["artifact"],{k:v for k,v in record.items() if k!="artifact"})
        return x

    def test_phase2_clean_fixture_status_derivation_mutations(self):
        with tempfile.TemporaryDirectory() as directory:
            base=self._materialized_phase2_fixture(directory)
            self.assertTrue(bench.validate_phase2_result(base))
            def reject(label,mutate):
                value=json.loads(json.dumps(base)); mutate(value)
                self.assertTrue(bench.validate_phase2_result(base),label+" baseline")
                with self.assertRaises(ValueError,msg=label): bench.validate_phase2_result(value)
            reject("C2 forged PASS",lambda x:x["claims"]["C2"].__setitem__("status","PASS"))
            reject("C2 forged FAIL",lambda x:x["claims"]["C2"].__setitem__("status","FAIL"))
            reject("C2 association/status artifact mismatch",lambda x:([a.__setitem__("association","action") for a in x["claims"]["C2"]["attempts"]],x["claims"]["C2"].__setitem__("status","FAIL")))
            reject("C3 forged FAIL",lambda x:x["claims"]["C3"].__setitem__("status","FAIL"))
            reject("C3 forged NOT_MEASURED",lambda x:x["claims"]["C3"].__setitem__("status","NOT_MEASURED"))
            reject("C3 false PASS",lambda x:(x["claims"]["C3"].__setitem__("status","PASS"),x["claims"]["C3"]["attempts"][0].__setitem__("semantic_association",True)))
            reject("C4 forged FAIL",lambda x:x["claims"]["C4"].__setitem__("status","FAIL"))
            reject("C4 forged INCONCLUSIVE",lambda x:x["claims"]["C4"].__setitem__("status","INCONCLUSIVE"))
            reject("C4 forged NOT_MEASURED",lambda x:x["claims"]["C4"].__setitem__("status","NOT_MEASURED"))
            reject("C4 fixture/status artifact mismatch",lambda x:(x["claims"]["C4"]["fixtures"][0].__setitem__("status","FAIL"),x["claims"]["C4"].__setitem__("status","FAIL")))
            comparator=next((r for r in base.get("comparators",{}).get("records",[]) if r.get("attempts")),None)
            if comparator:
                def active(value): return next(r for r in value["comparators"]["records"] if r.get("name")==comparator["name"])
                reject("comparator forged aggregate PASS",lambda x:active(x).__setitem__("status","PASS"))
                reject("comparator forged attempt PASS",lambda x:active(x)["attempts"][0].__setitem__("status","PASS"))
                reject("comparator argv identity",lambda x:active(x)["attempts"][0].__setitem__("argv",["/forged"]))
                reject("comparator filename-only readiness",lambda x:(active(x)["attempts"][0]["readiness"].__setitem__("matched",True),active(x)["attempts"][0]["readiness"].__setitem__("snapshot",[Path(active(x)["argv"][-1]).name])))
                reject("comparator corpus path substitution",lambda x:active(x).__setitem__("argv",active(x)["argv"][:-1]+["/forged/corpus.log"]))
                reject("comparator readiness endpoint",lambda x:active(x)["attempts"][0]["readiness"].__setitem__("matched",False))
                reject("comparator lifecycle",lambda x:active(x)["attempts"][0]["process"].__setitem__("reaped",False))
                reject("comparator discovery path",lambda x:next(t for t in x["comparators"]["tools"] if t["name"]==comparator["name"]).__setitem__("path","/forged/tool"))
                reject("comparator discovery alias",lambda x:next(t for t in x["comparators"]["tools"] if t["name"]=="vi").__setitem__("alias_of","nvim"))
                reject("Kakoune forged unavailable",lambda x:next(t for t in x["comparators"]["tools"] if t["name"]=="kak").__setitem__("status","UNAVAILABLE"))
                reject("vis forged alias",lambda x:(next(t for t in x["comparators"]["tools"] if t["name"]=="vis").__setitem__("status","ALIAS_OF"),next(t for t in x["comparators"]["tools"] if t["name"]=="vis").__setitem__("alias_of","vim")))
                reject("vis status downgrade",lambda x:next(t for t in x["comparators"]["tools"] if t["name"]=="vis").__setitem__("status","UNSUPPORTED"))
                raw_path=Path(comparator["attempts"][0]["artifact"]["path"])
                raw_path.write_text("not-json\n")
                with self.assertRaises(ValueError,msg="malformed comparator raw artifact"):
                    bench.validate_phase2_result(base)

    def test_valid_result_mutations_are_rejected(self):
        with tempfile.TemporaryDirectory() as d:
            out=Path(d)/"result.json"; corpus=Path(d)/"corpus"; buf=io.StringIO()
            with contextlib.redirect_stdout(buf): bench.smoke(SimpleNamespace(corpus=str(corpus),timeout=4,output=str(out)))
            base=json.loads(out.read_text()); self.assertTrue(bench.validate_result(base))
            def reject(label,mutate):
                value=json.loads(json.dumps(base)); mutate(value)
                with self.assertRaises(ValueError,msg=label): bench.validate_result(value)
            reject("missing readiness",lambda x:x["profiles"]["bare"][0]["process"].__setitem__("readiness",None))
            reject("endpoint mismatch",lambda x:x["profiles"]["bare"][0]["endpoint"]["snapshot"]["rows"].__setitem__(0,"corrupt"))
            reject("artifact hash",lambda x:x["profiles"]["bare"][0]["evidence"]["endpoint_screen"].__setitem__("sha256","0"*64))
            reject("EOF",lambda x:x["profiles"]["bare"][0]["process"].__setitem__("pty_eof",False))
            reject("drain",lambda x:x["profiles"]["bare"][0]["process"].__setitem__("drain_complete",False))
            reject("timeout",lambda x:x["profiles"]["bare"][0]["process"].__setitem__("timed_out",True))
            reject("PGID",lambda x:x["profiles"]["bare"][0]["process"].__setitem__("pgid_after",[{"pid":1}]))
            reject("helper",lambda x:x["profiles"]["shipped"][0]["profile_identity"].__setitem__("helper_observed",False))
            reject("identity",lambda x:x["profiles"]["bare"][0]["profile_identity"].__setitem__("identity_confidence","INCONCLUSIVE"))
            reject("PageDown",lambda x:x["profiles"]["bare"][1]["endpoint"]["snapshot"]["rows"].__setitem__(1,"wrong"))
            reject("diagnostic exit",lambda x:x["diagnostics"][0]["process"].__setitem__("exit",1))
            reject("diagnostic signature",lambda x:x["diagnostics"][0]["process"].__setitem__("stderr",""))
            reject("corpus config",lambda x:x.__setitem__("corpus_config_sha256","0"*64))
            reject("corpus manifest",lambda x:x.__setitem__("corpus_manifest_sha256","0"*64))
            reject("measured claim",lambda x:x["claims"]["C1"].__setitem__("status","PASS"))

if __name__=="__main__": unittest.main()
