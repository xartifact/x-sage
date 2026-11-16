import { describe, expect, test } from 'bun:test';
import { applyTheme, initializeTheme, validAppearance, validPalette } from './theme';

function environment(systemDark: boolean, values: Record<string,string> = {}, denied = false) {
 const keys = ['document','localStorage','matchMedia'] as const;
 const descriptors = keys.map(key => [key,Object.getOwnPropertyDescriptor(globalThis,key)] as const);
 const classes = new Set<string>();
 const element = {dataset:{} as Record<string,string>,style:{colorScheme:''},classList:{toggle(name:string,enabled:boolean){enabled ? classes.add(name) : classes.delete(name);}}};
 Object.defineProperty(globalThis,'document',{configurable:true,value:{documentElement:element}});
 Object.defineProperty(globalThis,'matchMedia',{configurable:true,value:()=>({matches:systemDark})});
 Object.defineProperty(globalThis,'localStorage',{configurable:true,value:{getItem(key:string){if(denied)throw new Error('Storage blocked');return values[key]??null;}}});
 return {element,classes,restore(){for(const [key,descriptor] of descriptors){if(descriptor)Object.defineProperty(globalThis,key,descriptor);else Reflect.deleteProperty(globalThis,key);}}};
}

describe('xartifact theme preferences',()=>{
 test('accepts supported values and rejects inherited properties and untrusted storage values',()=>{
  for(const palette of ['iris','ocean','jade','clay'] as const)expect(validPalette(palette)).toBe(palette);
  for(const input of ['__proto__','constructor','blue',null,42,{}])expect(validPalette(input)).toBe('iris');
  expect(validAppearance('dark')).toBe('dark');expect(validAppearance('light')).toBe('light');
  for(const input of ['system','invalid',null,{}])expect(validAppearance(input)).toBe('system');
 });
 test('each palette applies independently of explicit appearance or OS preference',()=>{
  for(const systemDark of [true,false]){
   const env=environment(systemDark);try{
    for(const palette of ['iris','ocean','jade','clay'] as const){
     for(const appearance of ['dark','light','system'] as const){
      applyTheme(palette,appearance);
      const dark=appearance==='dark'||(appearance==='system'&&systemDark);
      expect(env.element.dataset.xartifactPalette).toBe(palette);
      expect(env.classes.has('dark')).toBe(dark);
      expect(env.element.style.colorScheme).toBe(dark?'dark':'light');
     }
    }
   }finally{env.restore();}
  }
 });
 test('initialization restores persisted preferences and survives blocked storage',()=>{
  const stored=environment(true,{'xartifact.palette':'jade','xartifact.appearance':'light'});
  try{initializeTheme();expect(stored.element.dataset.xartifactPalette).toBe('jade');expect(stored.classes.has('dark')).toBe(false);}finally{stored.restore();}
  const denied=environment(true,{},true);
  try{initializeTheme();expect(denied.element.dataset.xartifactPalette).toBe('iris');expect(denied.classes.has('dark')).toBe(true);}finally{denied.restore();}
 });
});
