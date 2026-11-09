;; Iteration count over a 320x200 grid, 100 iterations max (doubles, loop/recur).
(defn iterations [^double cr ^double ci ^long max-iter]
  (loop [zr 0.0 zi 0.0 i 0]
    (if (or (>= i max-iter) (> (+ (* zr zr) (* zi zi)) 4.0))
      i
      (recur (+ (- (* zr zr) (* zi zi)) cr) (+ (* 2.0 zr zi) ci) (inc i)))))

(defn mandelbrot [w h max-iter]
  (loop [y 0 total 0]
    (if (< y h)
      (recur (inc y)
             (loop [x 0 total total]
               (if (< x w)
                 (let [cr (- (* 3.0 (/ x (double w))) 2.0)
                       ci (- (* 2.0 (/ y (double h))) 1.0)]
                   (recur (inc x) (+ total (iterations cr ci max-iter))))
                 total)))
      total)))

(println (mandelbrot 320 200 100))
