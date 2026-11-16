import { describe, expect, test } from 'bun:test';
import { mkdtemp, mkdir, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createApp } from './app.ts';

const concept = (id: string, layer = 2) => ({frontmatter:{id,atom:`Concept ${id}`,layer,domain:['engineering'],triggers:[id],confidence:'hypothetical',source:'personal',edges:[] as Array<{type:string;target:string;confidence:string}>},body:`# ${id}\n\nA concept body.`});
async function fixture() {
 const root=await mkdtemp(join(tmpdir(),'sage-workspace-'));
 for(const id of ['alpha','beta']) await mkdir(join(root,id,'concepts'),{recursive:true});
 const registryPath=join(root,'registry.json');
 await writeFile(registryPath,JSON.stringify({knowledgeBases:['alpha','beta'].map(id=>({id,name:id.toUpperCase(),path:id}))}));
 const app=createApp({kbRoot:join(root,'alpha'),registryPath,writeToken:'test-token'});
 const post=(kb:string,body:unknown,token='test-token')=>app.request(`/api/concepts?kb=${kb}`,{method:'POST',headers:{'Content-Type':'application/json',Authorization:`Bearer ${token}`},body:JSON.stringify(body)});
 return {root,app,post,cleanup:()=>rm(root,{recursive:true,force:true})};
}

describe('knowledge base workspace contracts',()=>{
 test('catalog exposes selectable IDs, preserves default, rejects unknown scope',async()=>{
  const f=await fixture();try{
   const data=await (await f.app.request('/api/knowledge-bases')).json() as {knowledgeBases:Array<{id:string;isDefault:boolean;root?:string}>};
   expect(data.knowledgeBases.map(x=>x.id)).toEqual(['alpha','beta']);
   expect(data.knowledgeBases[0]?.isDefault).toBe(true);
   expect(data.knowledgeBases[0]?.root).toBeUndefined();
   expect((await f.app.request('/api/viz?kb=unknown')).status).toBe(404);
   expect((await f.post('unknown',concept('new'))).status).toBe(404);
  }finally{await f.cleanup();}
 });
 test('create is immediately visible only in its scope, including default requests',async()=>{
  const f=await fixture();try{
   expect((await f.post('beta',concept('beta-only'))).status).toBe(201);
   const beta=await (await f.app.request('/api/viz?kb=beta')).json() as {nodes:Array<{id:string}>};
   const alpha=await (await f.app.request('/api/viz?kb=alpha')).json() as {nodes:Array<{id:string}>};
   const legacy=await (await f.app.request('/api/viz')).json() as {nodes:Array<{id:string}>};
   expect(beta.nodes.map(n=>n.id)).toEqual(['beta-only']);expect(alpha.nodes).toEqual([]);expect(legacy.nodes).toEqual([]);
   expect((await f.app.request('/api/concepts/beta-only/body?kb=alpha')).status).toBe(404);
   expect((await f.app.request('/api/concepts/beta-only/body?kb=beta')).status).toBe(200);
   expect((await f.post('alpha',concept('beta-only'))).status).toBe(201);
  }finally{await f.cleanup();}
 });
 test('duplicates cannot overwrite a body or create same ID in another layer',async()=>{
  const f=await fixture();try{
   expect((await f.post('alpha',concept('unique'))).status).toBe(201);
   expect((await f.post('alpha',{...concept('unique'),body:'replacement'})).status).toBe(409);
   expect((await f.post('alpha',concept('unique',3))).status).toBe(409);
   const saved=await (await f.app.request('/api/concepts/unique/body?kb=alpha')).json() as {body:string};
   expect(saved.body).toContain('A concept body.');
  }finally{await f.cleanup();}
 });
 test('simultaneous creation across layers has exactly one winner',async()=>{
  const f=await fixture();try{
   const results=await Promise.all([f.post('alpha',concept('race',2)),f.post('alpha',concept('race',3))]);
   expect(results.map(r=>r.status).sort()).toEqual([201,409]);
   const data=await (await f.app.request('/api/viz?kb=alpha')).json() as {nodes:Array<{id:string}>};
   expect(data.nodes.filter(n=>n.id==='race')).toHaveLength(1);
  }finally{await f.cleanup();}
 });
 test('invalid ID, metadata, body, missing targets and self edges are rejected without writes',async()=>{
  const f=await fixture();try{
   const missing=concept('missing-target');missing.frontmatter.edges=[{type:'depends-on',target:'absent',confidence:'confirmed'}];
   const self=concept('self');self.frontmatter.edges=[{type:'enables',target:'self',confidence:'confirmed'}];
   for(const body of [concept('../escape'),concept('invalid',7),{...concept('empty'),body:''},missing,self]) expect((await f.post('alpha',body)).status).toBe(400);
   const data=await (await f.app.request('/api/viz?kb=alpha')).json() as {nodes:unknown[]};expect(data.nodes).toEqual([]);
  }finally{await f.cleanup();}
 });
 test('write authentication applies to selected KB without restricting reads',async()=>{
  const f=await fixture();try{
   expect((await f.post('beta',concept('denied'),'incorrect')).status).toBe(401);
   expect((await f.app.request('/api/viz?kb=beta')).status).toBe(200);
   const data=await (await f.app.request('/api/viz?kb=beta')).json() as {nodes:unknown[]};expect(data.nodes).toEqual([]);
  }finally{await f.cleanup();}
 });
 test('legacy single-root server exposes default scope and accepts existing routes',async()=>{
  const f=await fixture();try{
   const app=createApp({kbRoot:join(f.root,'alpha')});
   const data=await (await app.request('/api/knowledge-bases')).json() as {knowledgeBases:Array<{id:string}>};expect(data.knowledgeBases.map(k=>k.id)).toEqual(['default']);
   expect((await app.request('/api/viz')).status).toBe(200);
   expect((await app.request('/api/viz?kb=default')).status).toBe(200);
   expect((await app.request('/api/viz?kb=beta')).status).toBe(404);
  }finally{await f.cleanup();}
 });
});

describe('workspace write boundary regressions',()=>{
 test('creation updates stored graph and manifest without stale nodes',async()=>{
  const f=await fixture();try{
   expect((await f.post('alpha',concept('base',0))).status).toBe(201);
   const child=concept('child');child.frontmatter.edges=[{type:'depends-on',target:'base',confidence:'confirmed'}];
   expect((await f.post('alpha',child)).status).toBe(201);
   const response=await f.app.request('/api/validate?kb=alpha&graph=true');
   expect(response.status).toBe(200);
   const report=await response.json() as {valid:boolean;graph:{staleness:{missing:string[];extra:string[];nodeDrift:boolean}}};
   expect(report.valid).toBe(true);
   expect(report.graph.staleness).toEqual({missing:[],extra:[],nodeDrift:false});
   const manifest=await Bun.file(join(f.root,'alpha','MANIFEST.md')).text();
   expect(manifest).toContain('base');expect(manifest).toContain('child');
   expect(await Bun.file(join(f.root,'beta','MANIFEST.md')).exists()).toBe(false);
  }finally{await f.cleanup();}
 });
 test('checkpoint rejects traversal and invalid session values for reads and writes',async()=>{
  const f=await fixture();try{
   for(const method of ['GET','POST','PUT']){
    const response=await f.app.request('/api/checkpoints/..%2Fescape?kb=alpha',{method,headers:{Authorization:'Bearer test-token','Content-Type':'application/json'},...(method==='GET'?{}:{body:'{}'})});
    expect(response.status).toBe(400);
   }
   for(const session of ['-1','0','1.5','NaN','1junk','9007199254740992']){
    expect((await f.app.request(`/api/checkpoints/base?kb=alpha&session=${session}`)).status).toBe(400);
   }
   for(const sessionNumber of [-1,0,1.5,'NaN',9007199254740992]){
    const response=await f.app.request('/api/checkpoints/base?kb=alpha',{method:'POST',headers:{Authorization:'Bearer test-token','Content-Type':'application/json'},body:JSON.stringify({sessionNumber})});
    expect(response.status).toBe(400);
   }
  }finally{await f.cleanup();}
 });
 test('malformed registry returns a safe failure without exposing filesystem paths',async()=>{
  const f=await fixture();try{
   await writeFile(join(f.root,'registry.json'),'{broken');
   const response=await f.app.request('/api/knowledge-bases');
   expect(response.status).toBeGreaterThanOrEqual(400);
   const text=await response.text();expect(text).not.toContain(f.root);expect(text).not.toContain('SyntaxError');
   expect(text).toContain('registry');
   const denied=await f.post('alpha',concept('should-not-write'));
   expect(denied.status).toBeGreaterThanOrEqual(400);
   expect(await Bun.file(join(f.root,'alpha','concepts','L2-support','should-not-write.md')).exists()).toBe(false);
  }finally{await f.cleanup();}
 });
});
