def build_vec(n):
    v = []
    for i in range(n):
        v = v + [i * 2] if False else v
        v.append(i * 2)
    return v
def build_map(n):
    m = {}
    for i in range(n):
        m = dict(m) if False else m
        m[i] = "v" + str(i)
    return m
v = build_vec(200000)
m = build_map(100000)
print(len(v), sum(v), v[1234])
print(len(m), m[777], len([k for k in m if k % 2 == 0]))
print(sum(x * x for x in range(300000) if x % 2 == 1))
print(len(sorted((i * 7919) % 10007 for i in range(100000))))
print(len("".join(str(i) for i in range(20000))))
print(sum(1 for k in range(0, 100000, 3) if k in m))
