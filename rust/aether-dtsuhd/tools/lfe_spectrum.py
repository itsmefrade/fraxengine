import sys,wave,numpy as np
w=wave.open(sys.argv[1]); n=w.getnframes(); nc=w.getnchannels()
x=np.frombuffer(w.readframes(n),dtype=np.int16).astype(np.float64).reshape(-1,nc)/32768
ch=x[:,0]
print('frames',n,'peak',abs(ch).max(),'rms',ch.std())
# active segments
fl=1024
rmsf=np.array([ch[i:i+fl].std() for i in range(0,len(ch)-fl,fl)])
act=np.where(rmsf>1e-5)[0]
print('active frames',len(act),'first/last',act[:3],act[-3:])
# PSD over active frames
from numpy.fft import rfft
acc=np.zeros(fl//2+1)
for i in act: 
    seg=ch[i*fl:(i+1)*fl]*np.hanning(fl); acc+=abs(rfft(seg))**2
acc/=max(1,len(act))
f=np.fft.rfftfreq(fl,1/48000)
tot=acc.sum()
for lo,hi in [(0,50),(50,100),(100,200),(200,300),(300,400),(400,600),(600,1000),(1000,5000),(5000,24000)]:
    m=(f>=lo)&(f<hi); print(f'{lo:5d}-{hi:5d} Hz: {10*np.log10(acc[m].sum()/tot+1e-30):7.1f} dB')
# discontinuity metric at frame boundaries
d=np.abs(np.diff(ch)); 
print('max sample step',d.max(),'median step',np.median(d[np.abs(ch[1:])>1e-5]) if (np.abs(ch)>1e-5).any() else 0)
