def iterations(cr, ci, max_iter):
    zr = zi = 0.0
    i = 0
    while i < max_iter and zr * zr + zi * zi <= 4.0:
        zr, zi = zr * zr - zi * zi + cr, 2.0 * zr * zi + ci
        i += 1
    return i

def mandelbrot(w, h, max_iter):
    total = 0
    for y in range(h):
        for x in range(w):
            cr = 3.0 * (x / float(w)) - 2.0
            ci = 2.0 * (y / float(h)) - 1.0
            total += iterations(cr, ci, max_iter)
    return total

print(mandelbrot(320, 200, 100))
