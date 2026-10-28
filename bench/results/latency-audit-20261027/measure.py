import json, subprocess, resource, statistics, time, random
binary='/home/keegan/small/bin-e29b/scout'
index='/mnt/ebs/idx-e29'
queries=[('common',['-F'],'std::unique_ptr'),('license',['-F'],'Use of this source code is governed by a BSD-style license'),('alternation',[],'TODO|FIXME|HACK'),('date',[],r'\d{4}-\d{2}-\d{2}'),('short',[],'if')]
rows=[]
for name,flags,pattern in queries:
    modes=[('lines',[]),('files',['-l']),('count',['-c'])]
    if name=='short': modes=[('files',['-l'])]
    for mode,more in modes:
        samples=[]
        stats=''
        for rep in range(4):
            before=resource.getrusage(resource.RUSAGE_CHILDREN)
            start=time.perf_counter()
            p=subprocess.run([binary,'--index-dir',index,'search','--freshness','off','--threads','2','--stats','-n',*flags,*more,pattern],stdout=subprocess.DEVNULL,stderr=subprocess.PIPE,text=True,timeout=90)
            after=resource.getrusage(resource.RUSAGE_CHILDREN)
            if p.returncode: raise RuntimeError(p.stderr)
            if rep:
                samples.append({'wall_ms':(time.perf_counter()-start)*1000,'cpu_ms':((after.ru_utime+after.ru_stime)-(before.ru_utime+before.ru_stime))*1000,'read_mib':(after.ru_inblock-before.ru_inblock)*512/2**20})
            stats=p.stderr.strip()
        row={'query':name,'mode':mode,'samples':samples,'median_wall_ms':statistics.median(s['wall_ms'] for s in samples),'median_cpu_ms':statistics.median(s['cpu_ms'] for s in samples),'stats_last':stats}
        rows.append(row)
        print(json.dumps(row),flush=True)
print(json.dumps({'complete':True,'binary':binary,'index':index,'rows':rows}),flush=True)
