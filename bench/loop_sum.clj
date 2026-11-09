;; Tight integer loop: sum of i*i mod 7 for i below 10 million.
(defn sum-to [n]
  (loop [i 0 acc 0]
    (if (< i n)
      (recur (inc i) (+ acc (rem (* i i) 7)))
      acc)))
(println (sum-to 10000000))
