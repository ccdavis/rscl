(ns mylib.util
  (:require [mylib.strings :as s]))
(defn double-it [x] (* 2 x))
(defn shout [x] (str (s/wrap (clojure.string/upper-case x)) "!"))
(defmacro twice [form] `(do ~form ~form))
