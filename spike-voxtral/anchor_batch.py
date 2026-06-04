import time, sys
from pathlib import Path
import soundfile as sf
from mlx_audio.stt.utils import load_model
M="mlx-community/Voxtral-Mini-4B-Realtime-2602-4bit"
fx=Path("tests/fixtures/voice/monologue_5min.wav")
audio,sr=sf.read(fx,dtype="float32")
print(f"load...",flush=True); t=time.monotonic()
m=load_model(M); print(f"loaded {time.monotonic()-t:.1f}s, audio {len(audio)/sr:.1f}s",flush=True)
t=time.monotonic()
out=m.generate(audio, temperature=0.0)
print(f"batch generate {time.monotonic()-t:.1f}s",flush=True)
txt=out.text.strip()
Path("spike-voxtral/results").mkdir(parents=True,exist_ok=True)
Path("spike-voxtral/results/batch_transcript.txt").write_text(txt+"\n")
w=txt.split()
print("WORDS:",len(w))
print("FIRST 15:"," ".join(w[:15]))
print("LAST 15 :"," ".join(w[-15:]))
