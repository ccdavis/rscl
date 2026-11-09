def sum_to(n):
    i = 0
    acc = 0
    while i < n:
        acc += (i * i) % 7
        i += 1
    return acc
print(sum_to(10000000))
