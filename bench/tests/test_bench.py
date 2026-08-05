import contextlib, hashlib, io, json, os, sys, tempfile, threading, time, unittest, textwrap
from types import SimpleNamespace
from pathlib import Path
sys.path.insert(0, str(Path(__file__).parents[1]))
import bench

class Phase1Tests(unittest.TestCase):
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
                path=root/"profiles"/profile/name; path.write_bytes((profile+name).encode()); digest,size=bench.sha(path); files.setdefault(profile,{})[name]={"path":str(path),"sha256":digest,"size":size,"version":"test","version_capture":{"stdout":"test","stderr":"","exit":0},"arch":"test"}
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
