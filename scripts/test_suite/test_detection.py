import subprocess as sp
from time import sleep

sp.run(["cargo","build"])
h2 = sp.Popen(["cargo","run","--", "-c","shadowquic/config_examples/server.yaml"],
                     start_new_session=True )
sleep(0.5)
out = sp.run(["curl","https://echo.free.beeceptor.com:1443",
               "--http3-only",
               "--resolve",
               "-v",
               "--trace-time",
               "--resolve",
               "echo.free.beeceptor.com:1443:127.0.0.1"
               ],
             capture_output=True,
             text=True)


h2.terminate()
print(out.stderr)

print(out.stdout)
assert("GET" in out.stdout)