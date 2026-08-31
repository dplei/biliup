#!/usr/bin/env python3
"""Prepare isolated Agent inputs and validate cited reports; never starts an Agent or executes logs."""
import argparse
import json
from pathlib import Path
import shutil
import evidence as e

QUESTIONS = ["Q1-recording", "Q2-media", "Q3-processing", "Q4-upload", "Q5-submission", "Q6-unknowns"]
PROMPTS = e.ROOT / '.scratch/structured-logging/prompts'


def prepare(bundle, output):
    bundle, output = Path(bundle), Path(output).resolve()
    if output.exists() or not output.is_relative_to((e.ROOT/'data/observability-evidence').resolve()):
        raise ValueError('new_private_output_required')
    m = e.read_json(bundle/'manifest.json')
    validation = e.validate(bundle)
    if any(x['code'] in {'checksum_mismatch','invalid_file_reference','duplicate_reference'} for x in validation['errors']):
        raise ValueError('package_integrity_failed')
    for kind, files in [('old',['legacy.jsonl']),('new',['native.jsonl'])]:
        dest=output/kind; dest.mkdir(parents=True,mode=0o700)
        # Each initial context only receives its own source metadata and explicit inventory.
        view={key:m[key] for key in ['version','source_version','schema_version','catalog_version','scope','sampling','limits']}
        view['source']=kind
        view['questions']=QUESTIONS
        if kind=='old':
            view['legacy']=m['legacy']
            view['filter']=m.get('capture_config',{}).get('legacy_filter','unknown')
        else:
            view['native_coverage']=m['native_coverage']
            view['database']=m['database'];view['health']=m['health']
            view['filter']=m.get('capture_config',{}).get('new_filter','unknown')
            view['completeness']=m['completeness']
        view['files']={f:m['files'][f] for f in files if f in m['files']}
        for name in view['files']:
            shutil.copyfile(bundle/name,dest/name)
        (dest/'manifest.json').write_bytes(e.encoded(view))
        shutil.copyfile(PROMPTS/(kind+'.md'),dest/'prompt.md')
    # No cross directory exists until both independently completed reports have been provided.
    return {'old':str(output/'old'),'new':str(output/'new'),'cross':'pending_two_independent_reports'}


def check_report(report, refs, source):
    errors=[]
    if report.get('source')!=source:
        errors.append('wrong_source')
    if report.get('status') not in {'passed','failed','insufficient'}:
        errors.append('invalid_status')
    questions={r.get('question') for r in report.get('answers',[])}
    if set(QUESTIONS)!=questions or len(report.get('answers',[]))!=len(QUESTIONS):
        errors.append('question_inventory_incomplete')
    for answer in report.get('answers',[]):
        if answer.get('status') not in {'confirmed','inferred','unknown','pending','not-applicable'}:
            errors.append('invalid_fact_status')
        if answer.get('status') in {'confirmed','inferred'} and not answer.get('refs'):
            errors.append('uncited_claim')
        if not set(answer.get('refs',[])) <= refs:
            errors.append('invalid_or_cross_source_reference')
        if not isinstance(answer.get('unknown_fields'),list):
            errors.append('unknown_fields_required')
    return sorted(set(errors))


def cross(bundle, output, old_report, new_report):
    bundle,output=Path(bundle),Path(output).resolve()
    if output.exists() or not output.is_relative_to((e.ROOT/'data/observability-evidence').resolve()):
        raise ValueError('new_private_output_required')
    reports={'old':e.read_json(old_report),'new':e.read_json(new_report)}
    validation=e.validate(bundle)
    if any(x['code'] in {'checksum_mismatch','invalid_file_reference','duplicate_reference'} for x in validation['errors']):
        raise ValueError('package_integrity_failed')
    for kind,name in [('old','legacy.jsonl'),('new','native.jsonl')]:
        refs=set()
        if (bundle/name).exists():
            with (bundle/name).open() as f:
                refs={json.loads(line)['ref'] for line in f}
        errors=check_report(reports[kind],refs,kind)
        if errors:
            raise ValueError(kind+':'+','.join(errors))
    output.mkdir(parents=True,mode=0o700)
    manifest=e.read_json(bundle/'manifest.json')
    for name in ['manifest.json',*manifest['files']]:
        if name not in {'manifest.json','legacy.jsonl','native.jsonl','bridge.jsonl','business.jsonl'}:
            raise ValueError('invalid_source_file')
        shutil.copyfile(bundle/name,output/name)
    for kind,report in reports.items():
        (output/(kind+'-report.json')).write_bytes(e.encoded(report))
    (output/'validation.json').write_bytes(e.encoded(validation))
    shutil.copyfile(PROMPTS/'cross.md',output/'prompt.md')
    return {'cross':str(output),'independence':'must_be_attested_by_controller; cannot_be_proven_from_report_text'}


def bridge_transport(bundle, controlled=False):
    import time
    bundle=Path(bundle)
    validation=e.validate(bundle)
    if validation['status']!='passed':
        return {'status':validation['status'],'errors':validation['errors'],'native_coverage':'not-started'}
    def lines(name):
        path=bundle/name
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []
    old,bridge=lines('legacy.jsonl'),lines('bridge.jsonl')
    start=time.monotonic();pairs=[];ambiguous=[];missing=[];used=set()
    for row in bridge:
        if time.monotonic()-start>e.MAX_SECONDS:
            return {'status':'insufficient','reason':'comparison_deadline','native_coverage':'not-started'}
        message=row['event']['message']
        candidates=[r['ref'] for r in old if message and message!='[REDACTED]' and message in r['text']]
        if len(candidates)==1 and candidates[0] not in used:
            pairs.append({'bridge_ref':row['ref'],'legacy_ref':candidates[0]});used.add(candidates[0])
        elif candidates or message=='[REDACTED]':
            ambiguous.append({'bridge_ref':row['ref'],'candidate_refs':candidates[:20]})
        else:
            missing.append({'bridge_ref':row['ref'],'reason':'message_not_found'})
    status='failed' if missing else 'insufficient' if ambiguous or not controlled or not bridge else 'passed'
    return {'version':'reconciliation-v1','status':status,'scope':'controlled_message_transport' if controlled else 'weak_message_candidates_only','native_coverage':'not-started','pairs':pairs,'ambiguous':ambiguous,'missing':missing,'unmatched_old_refs':[r['ref'] for r in old if r['ref'] not in used],'limitations':['text matches prove no business identity or causality','unknown legacy fields remain unknown','human independent reviews still required']}


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    subs=parser.add_subparsers(dest='command',required=True)
    p=subs.add_parser('prepare');p.add_argument('bundle',type=Path);p.add_argument('output',type=Path)
    p=subs.add_parser('cross');p.add_argument('bundle',type=Path);p.add_argument('output',type=Path);p.add_argument('old_report',type=Path);p.add_argument('new_report',type=Path)
    p=subs.add_parser('transport');p.add_argument('bundle',type=Path);p.add_argument('--controlled',action='store_true')
    args=parser.parse_args()
    if args.command=='transport':
        print(json.dumps(bridge_transport(args.bundle,args.controlled),ensure_ascii=False,indent=2))
        return
    result=prepare(args.bundle,args.output) if args.command=='prepare' else cross(args.bundle,args.output,args.old_report,args.new_report)
    print(json.dumps(result,indent=2))

if __name__=='__main__':
    main()
